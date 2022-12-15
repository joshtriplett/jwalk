#![warn(clippy::all)]

//! Filesystem walk.
//!
//! - Performed in parallel using rayon
//! - Entries streamed in sorted order
//! - Custom sort/filter/skip/state
//!
//! # Example
//!
//! Recursively iterate over the "foo" directory sorting by name:
//!
//! ```no_run
//! # use std::io::Error;
//! use jwalk::{WalkDir};
//!
//! # fn try_main() -> Result<(), Error> {
//! for entry in WalkDir::new("foo").sort(true) {
//!   println!("{}", entry?.path().display());
//! }
//! # Ok(())
//! # }
//! ```
//! # Extended Example
//!
//! This example uses the
//! [`process_read_dir`](struct.WalkDirGeneric.html#method.process_read_dir)
//! callback for custom:
//! 1. **Sort** Entries by name
//! 2. **Filter** Errors and hidden files
//! 3. **Skip** Content of directories at depth 2
//! 4. **State** Track depth `read_dir_state`. Mark first entry in each
//!    directory with [`client_state`](struct.DirEntry.html#field.client_state)
//!    `= true`.
//!
//! ```no_run
//! # use std::io::Error;
//! use std::cmp::Ordering;
//! use jwalk::{ WalkDirGeneric };
//!
//! # fn try_main() -> Result<(), Error> {
//! let walk_dir = WalkDirGeneric::<((usize),(bool))>::new("foo")
//!     .process_read_dir(|depth, path, read_dir_state, children| {
//!         // 1. Custom sort
//!         children.sort_by(|a, b| match (a, b) {
//!             (Ok(a), Ok(b)) => a.file_name.cmp(&b.file_name),
//!             (Ok(_), Err(_)) => Ordering::Less,
//!             (Err(_), Ok(_)) => Ordering::Greater,
//!             (Err(_), Err(_)) => Ordering::Equal,
//!         });
//!         // 2. Custom filter
//!         children.retain(|dir_entry_result| {
//!             dir_entry_result.as_ref().map(|dir_entry| {
//!                 dir_entry.file_name
//!                     .to_str()
//!                     .map(|s| s.starts_with('.'))
//!                     .unwrap_or(false)
//!             }).unwrap_or(false)
//!         });
//!         // 3. Custom skip
//!         children.iter_mut().for_each(|dir_entry_result| {
//!             if let Ok(dir_entry) = dir_entry_result {
//!                 if dir_entry.depth == 2 {
//!                     dir_entry.read_children_path = None;
//!                 }
//!             }
//!         });
//!         // 4. Custom state
//!         *read_dir_state += 1;
//!         children.first_mut().map(|dir_entry_result| {
//!             if let Ok(dir_entry) = dir_entry_result {
//!                 dir_entry.client_state = true;
//!             }
//!         });
//!     });
//!
//! for entry in walk_dir {
//!   println!("{}", entry?.path().display());
//! }
//! # Ok(())
//! # }
//! ```
//! # Inspiration
//!
//! This crate is inspired by both [`walkdir`](https://crates.io/crates/walkdir)
//! and [`ignore`](https://crates.io/crates/ignore). It attempts to combine the
//! parallelism of `ignore` with `walkdir`'s streaming iterator API. Some code,
//! comments, and test are copied directly from `walkdir`.
//!
//! # Implementation
//!
//! The following structures are central to the implementation:
//!
//! ## `ReadDirSpec`
//!
//! Specification of a future `read_dir` operation. These are stored in the
//! `read_dir_spec_queue` in depth first order. When a rayon thread is ready for
//! work it pulls the first availible `ReadDirSpec` from this queue.
//!
//! ## `ReadDir`
//!
//! Result of a `read_dir` operation generated by rayon thread. These results
//! are stored in the `read_dir_result_queue`, also depth first ordered.
//!
//! ## `ReadDirIter`
//!
//! Pulls `ReadDir` results from the `read_dir_result_queue`. This iterator is
//! driven by calling thread. Results are returned in strict depth first order.
//!
//! ## `DirEntryIter`
//!
//! Wraps a `ReadDirIter` and yields individual `DirEntry` results in strict
//! depth first order.

mod core;

use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cmp::Ordering;
use std::default::Default;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::{ReadDir, ReadDirSpec};

pub use crate::core::{DirEntry, DirEntryIter, Error};

/// Builder for walking a directory.
pub type WalkDir = WalkDirGeneric<((), ())>;

/// A specialized Result type for WalkDir.
pub type Result<T> = std::result::Result<T, Error>;

/// Client state maintained while performing walk.
///
/// for state stored in DirEntry's
/// [`client_state`](struct.DirEntry.html#field.client_state) field.
///
/// Client state can be stored from within the
/// [`process_read_dir`](struct.WalkDirGeneric.html#method.process_read_dir) callback.
/// The type of ClientState is determined by WalkDirGeneric type parameter.
pub trait ClientState: Send + Default + Debug + 'static {
    type ReadDirState: Clone + Send + Default + Debug + 'static;
    type DirEntryState: Send + Default + Debug + 'static;
}

/// Generic builder for walking a directory.
///
/// [`ClientState`](trait.ClientState.html) type parameter allows you to specify
/// state to be stored with each DirEntry from within the
/// [`process_read_dir`](struct.WalkDirGeneric.html#method.process_read_dir)
/// callback.
///
/// Use [`WalkDir`](type.WalkDir.html) if you don't need to store client state
/// into yeilded DirEntries.
pub struct WalkDirGeneric<C: ClientState> {
    root: PathBuf,
    options: WalkDirOptions<C>,
}

type ProcessReadDirFunction<C> = dyn Fn(Option<usize>, &Path, &mut <C as ClientState>::ReadDirState, &mut Vec<Result<DirEntry<C>>>)
    + Send
    + Sync
    + 'static;

/// Degree of parallelism to use when performing walk.
///
/// Parallelism happens at the directory level. It will help when walking deep
/// filesystems with many directories. It wont help when reading a single
/// directory with many files.
///
/// If you plan to perform lots of per file processing you might want to use Rayon to
#[derive(Clone)]
pub enum Parallelism {
    /// Run on calling thread, similar to what happens in the `walkdir` crate.
    Serial,
    /// Run in default rayon thread pool.
    RayonDefaultPool {
        /// Define when we consider the rayon default pool too busy to serve our iteration and abort the iteration, defaulting to 1s.
        ///
        /// This can happen if `jwalk` is launched from within a par-iter on a pool that only has a single thread,
        /// or if there are many parallel `jwalk` invocations that all use the same threadpool, rendering it too busy
        /// to respond within this duration.
        busy_timeout: std::time::Duration,
    },
    /// Run in existing rayon thread pool
    RayonExistingPool {
        /// The pool to spawn our work onto.
        pool: Arc<ThreadPool>,
        /// Similar to [`Parallelism::RayonDefaultPool::busy_timeout`].
        busy_timeout: std::time::Duration,
    },
    /// Run in new rayon thread pool with # threads
    RayonNewPool(usize),
}

struct WalkDirOptions<C: ClientState> {
    sort: bool,
    min_depth: usize,
    max_depth: usize,
    skip_hidden: bool,
    follow_links: bool,
    parallelism: Parallelism,
    root_read_dir_state: C::ReadDirState,
    process_read_dir: Option<Arc<ProcessReadDirFunction<C>>>,
}

impl<C: ClientState> WalkDirGeneric<C> {
    /// Create a builder for a recursive directory iterator starting at the file
    /// path root. If root is a directory, then it is the first item yielded by
    /// the iterator. If root is a file, then it is the first and only item
    /// yielded by the iterator.
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        WalkDirGeneric {
            root: root.as_ref().to_path_buf(),
            options: WalkDirOptions {
                sort: false,
                min_depth: 0,
                max_depth: ::std::usize::MAX,
                skip_hidden: true,
                follow_links: false,
                parallelism: Parallelism::RayonDefaultPool {
                    busy_timeout: std::time::Duration::from_secs(1),
                },
                root_read_dir_state: C::ReadDirState::default(),
                process_read_dir: None,
            },
        }
    }

    /// Root path of the walk.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sort entries by `file_name` per directory. Defaults to `false`. Use
    /// [`process_read_dir`](struct.WalkDirGeneric.html#method.process_read_dir) for custom
    /// sorting or filtering.
    pub fn sort(mut self, sort: bool) -> Self {
        self.options.sort = sort;
        self
    }

    /// Skip hidden entries. Enabled by default.
    pub fn skip_hidden(mut self, skip_hidden: bool) -> Self {
        self.options.skip_hidden = skip_hidden;
        self
    }

    /// Follow symbolic links. By default, this is disabled.
    ///
    /// When `yes` is `true`, symbolic links are followed as if they were normal
    /// directories and files. If a symbolic link is broken or is involved in a
    /// loop, an error is yielded.
    ///
    /// When enabled, the yielded [`DirEntry`] values represent the target of
    /// the link while the path corresponds to the link. See the [`DirEntry`]
    /// type for more details.
    ///
    /// [`DirEntry`]: struct.DirEntry.html
    pub fn follow_links(mut self, follow_links: bool) -> Self {
        self.options.follow_links = follow_links;
        self
    }

    /// Set the minimum depth of entries yielded by the iterator.
    ///
    /// The smallest depth is `0` and always corresponds to the path given
    /// to the `new` function on this type. Its direct descendents have depth
    /// `1`, and their descendents have depth `2`, and so on.
    pub fn min_depth(mut self, depth: usize) -> Self {
        self.options.min_depth = depth;
        if self.options.min_depth > self.options.max_depth {
            self.options.min_depth = self.options.max_depth;
        }
        self
    }

    /// Set the maximum depth of entries yield by the iterator.
    ///
    /// The smallest depth is `0` and always corresponds to the path given
    /// to the `new` function on this type. Its direct descendents have depth
    /// `1`, and their descendents have depth `2`, and so on.
    ///
    /// A depth < 2 will automatically change `parallelism` to
    /// `Parallelism::Serial`. Parrallelism happens at the `fs::read_dir` level.
    /// It only makes sense to use multiple threads when reading more then one
    /// directory.
    ///
    /// Note that this will not simply filter the entries of the iterator, but
    /// it will actually avoid descending into directories when the depth is
    /// exceeded.
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.options.max_depth = depth;
        if self.options.max_depth < self.options.min_depth {
            self.options.max_depth = self.options.min_depth;
        }
        if self.options.max_depth < 2 {
            self.options.parallelism = Parallelism::Serial;
        }
        self
    }

    /// Degree of parallelism to use when performing walk. Defaults to
    /// [`Parallelism::RayonDefaultPool`](enum.Parallelism.html#variant.RayonDefaultPool).
    pub fn parallelism(mut self, parallelism: Parallelism) -> Self {
        self.options.parallelism = parallelism;
        self
    }

    /// Initial ClientState::ReadDirState that is passed to
    /// [`process_read_dir`](struct.WalkDirGeneric.html#method.process_read_dir)
    /// when processing root. Defaults to ClientState::ReadDirState::default().
    pub fn root_read_dir_state(mut self, read_dir_state: C::ReadDirState) -> Self {
        self.options.root_read_dir_state = read_dir_state;
        self
    }

    /// A callback function to process (sort/filter/skip/state) each directory
    /// of entries before they are yielded. Modify the given array to
    /// sort/filter entries. Use [`entry.read_children_path =
    /// None`](struct.DirEntry.html#field.read_children_path) to yield a
    /// directory entry but skip reading its contents. Use
    /// [`entry.client_state`](struct.DirEntry.html#field.client_state)
    /// to store custom state with an entry.
    pub fn process_read_dir<F>(mut self, process_by: F) -> Self
    where
        F: Fn(Option<usize>, &Path, &mut C::ReadDirState, &mut Vec<Result<DirEntry<C>>>)
            + Send
            + Sync
            + 'static,
    {
        self.options.process_read_dir = Some(Arc::new(process_by));
        self
    }
}

fn process_dir_entry_result<C: ClientState>(
    dir_entry_result: Result<DirEntry<C>>,
    follow_links: bool,
) -> Result<DirEntry<C>> {
    match dir_entry_result {
        Ok(mut dir_entry) => {
            if follow_links && dir_entry.file_type.is_symlink() {
                dir_entry = dir_entry.follow_symlink()?;
            }

            if dir_entry.depth == 0 && dir_entry.file_type.is_symlink() {
                // As a special case, if we are processing a root entry, then we
                // always follow it even if it's a symlink and follow_links is
                // false. We are careful to not let this change the semantics of
                // the DirEntry however. Namely, the DirEntry should still
                // respect the follow_links setting. When it's disabled, it
                // should report itself as a symlink. When it's enabled, it
                // should always report itself as the target.
                let metadata = fs::metadata(dir_entry.path())
                    .map_err(|err| Error::from_path(0, dir_entry.path(), err))?;
                if metadata.file_type().is_dir() {
                    dir_entry.read_children_path = Some(Arc::from(dir_entry.path()));
                }
            }

            Ok(dir_entry)
        }
        Err(err) => Err(err),
    }
}

impl<C: ClientState> IntoIterator for WalkDirGeneric<C> {
    type Item = Result<DirEntry<C>>;
    type IntoIter = DirEntryIter<C>;

    fn into_iter(self) -> DirEntryIter<C> {
        let sort = self.options.sort;
        let max_depth = self.options.max_depth;
        let min_depth = self.options.min_depth;
        let parallelism = self.options.parallelism;
        let skip_hidden = self.options.skip_hidden;
        let follow_links = self.options.follow_links;
        let process_read_dir = self.options.process_read_dir.clone();
        let mut root_read_dir_state = self.options.root_read_dir_state;
        let follow_link_ancestors = if follow_links {
            Arc::new(vec![Arc::from(self.root.clone()) as Arc<Path>])
        } else {
            Arc::new(vec![])
        };

        let root_entry = DirEntry::from_path(0, &self.root, false, follow_link_ancestors);
        let root_parent_path = root_entry
            .as_ref()
            .map(|root| root.parent_path().to_owned())
            .unwrap_or_default();
        let mut root_entry_results = vec![process_dir_entry_result(root_entry, follow_links)];
        if let Some(process_read_dir) = process_read_dir.as_ref() {
            process_read_dir(
                None,
                &root_parent_path,
                &mut root_read_dir_state,
                &mut root_entry_results,
            );
        }

        DirEntryIter::new(
            root_entry_results,
            parallelism,
            min_depth,
            root_read_dir_state,
            Arc::new(move |read_dir_spec| {
                let ReadDirSpec {
                    path,
                    depth,
                    mut client_read_state,
                    mut follow_link_ancestors,
                } = read_dir_spec;

                let read_dir_depth = depth;
                let read_dir_contents_depth = depth + 1;

                if read_dir_contents_depth > max_depth {
                    return Ok(ReadDir::new(client_read_state, Vec::new()));
                }

                follow_link_ancestors = if follow_links {
                    let mut ancestors = Vec::with_capacity(follow_link_ancestors.len() + 1);
                    ancestors.extend(follow_link_ancestors.iter().cloned());
                    ancestors.push(path.clone());
                    Arc::new(ancestors)
                } else {
                    follow_link_ancestors
                };

                let mut dir_entry_results: Vec<_> = fs::read_dir(path.as_ref())
                    .map_err(|err| Error::from_path(0, path.to_path_buf(), err))?
                    .filter_map(|dir_entry_result| {
                        let fs_dir_entry = match dir_entry_result {
                            Ok(fs_dir_entry) => fs_dir_entry,
                            Err(err) => {
                                return Some(Err(Error::from_io(read_dir_contents_depth, err)))
                            }
                        };

                        let dir_entry = match DirEntry::from_entry(
                            read_dir_contents_depth,
                            path.clone(),
                            &fs_dir_entry,
                            follow_link_ancestors.clone(),
                        ) {
                            Ok(dir_entry) => dir_entry,
                            Err(err) => return Some(Err(err)),
                        };

                        if skip_hidden && is_hidden(&dir_entry.file_name) {
                            return None;
                        }

                        Some(process_dir_entry_result(Ok(dir_entry), follow_links))
                    })
                    .collect();

                if sort {
                    dir_entry_results.sort_by(|a, b| match (a, b) {
                        (Ok(a), Ok(b)) => a.file_name.cmp(&b.file_name),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => Ordering::Equal,
                    });
                }

                if let Some(process_read_dir) = process_read_dir.as_ref() {
                    process_read_dir(
                        Some(read_dir_depth),
                        path.as_ref(),
                        &mut client_read_state,
                        &mut dir_entry_results,
                    );
                }

                Ok(ReadDir::new(client_read_state, dir_entry_results))
            }),
        )
    }
}

impl<C: ClientState> Clone for WalkDirOptions<C> {
    fn clone(&self) -> WalkDirOptions<C> {
        WalkDirOptions {
            sort: false,
            min_depth: self.min_depth,
            max_depth: self.max_depth,
            skip_hidden: self.skip_hidden,
            follow_links: self.follow_links,
            parallelism: self.parallelism.clone(),
            root_read_dir_state: self.root_read_dir_state.clone(),
            process_read_dir: self.process_read_dir.clone(),
        }
    }
}

impl Parallelism {
    pub(crate) fn spawn<OP>(&self, op: OP)
    where
        OP: FnOnce() + Send + 'static,
    {
        match self {
            Parallelism::Serial => op(),
            Parallelism::RayonDefaultPool { .. } => rayon::spawn(op),
            Parallelism::RayonNewPool(num_threads) => {
                let mut thread_pool = ThreadPoolBuilder::new();
                if *num_threads > 0 {
                    thread_pool = thread_pool.num_threads(*num_threads);
                }
                if let Ok(thread_pool) = thread_pool.build() {
                    thread_pool.spawn(op);
                } else {
                    rayon::spawn(op);
                }
            }
            Parallelism::RayonExistingPool { pool, .. } => pool.spawn(op),
        }
    }

    pub(crate) fn timeout(&self) -> Option<std::time::Duration> {
        match self {
            Parallelism::Serial | Parallelism::RayonNewPool(_) => None,
            Parallelism::RayonDefaultPool { busy_timeout }
            | Parallelism::RayonExistingPool { busy_timeout, .. } => Some(*busy_timeout),
        }
    }
}

fn is_hidden(file_name: &OsStr) -> bool {
    file_name
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

impl<B, E> ClientState for (B, E)
where
    B: Clone + Send + Default + Debug + 'static,
    E: Send + Default + Debug + 'static,
{
    type ReadDirState = B;
    type DirEntryState = E;
}
