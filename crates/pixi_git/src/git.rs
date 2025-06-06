/// Derived from `uv-git` implementation
/// Source: https://github.com/astral-sh/uv/blob/4b8cc3e29e4c2a6417479135beaa9783b05195d3/crates/uv-git/src/git.rs
/// This module represents all necessary git types and operations to interact with git repositories.
/// Example:
///   * `GitReference` that can represent a branch, tag, commit, or named ref.
///   * `GitRemote` that represents a remote repository and can be fetched somewhere on the local filesystem.
///   * `GitDatabase` and `GitRepository` that represents a local clone of a remote repository's database.
use std::{
    fmt::Display,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::LazyLock,
};

use reqwest::StatusCode;
use reqwest_middleware::ClientWithMiddleware;
use url::Url;

use crate::{
    GitError,
    sha::{GitOid, GitSha},
};

/// A file indicates that if present, `git reset` has been done and a repo
/// checkout is ready to go. See [`GitCheckout::reset`] for why we need this.
const CHECKOUT_READY_LOCK: &str = ".ok";
pub const GIT_DIR: &str = "GIT_DIR";

#[derive(Debug, thiserror::Error, Clone)]
pub enum GitBinaryError {
    #[error("Git executable not found. Ensure that Git is installed and available.")]
    GitNotFound,
    #[error(transparent)]
    Other(#[from] which::Error),
}

/// A global cache of the result of `which git`.
pub static GIT: LazyLock<Result<PathBuf, GitBinaryError>> = LazyLock::new(|| {
    which::which("git").map_err(|e| match e {
        which::Error::CannotFindBinaryPath => GitBinaryError::GitNotFound,
        e => GitBinaryError::Other(e),
    })
});

/// Strategy when fetching refspecs for a [`GitReference`]
enum RefspecStrategy {
    // All refspecs should be fetched, if any fail then the fetch will fail
    All,
    // Stop after the first successful fetch, if none succeed then the fetch will fail
    First,
}

/// A reference to commit or commit-ish.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum GitReference {
    /// A specific branch.
    Branch(String),
    /// A specific tag.
    Tag(String),
    /// A specific (short) commit.
    ShortCommit(String),
    /// From a reference that's ambiguously a branch or tag.
    BranchOrTag(String),
    /// From a reference that's ambiguously a short commit, a branch, or a tag.
    BranchOrTagOrCommit(String),
    /// From a named reference, like `refs/pull/493/head`.
    NamedRef(String),
    /// From a specific revision, using a full 40-character commit hash.
    FullCommit(String),
    /// The default branch of the repository, the reference named `HEAD`.
    #[default]
    DefaultBranch,
}

impl GitReference {
    /// Creates a [`GitReference`] from an arbitrary revision string, which could represent a
    /// branch, tag, commit, or named ref.
    pub fn from_rev(rev: String) -> Self {
        if rev.starts_with("refs/") {
            Self::NamedRef(rev)
        } else if GitReference::looks_like_commit_hash(&rev) {
            if rev.len() == 40 {
                Self::FullCommit(rev)
            } else {
                Self::BranchOrTagOrCommit(rev)
            }
        } else {
            Self::BranchOrTag(rev)
        }
    }

    /// Converts the [`GitReference`] to a `str`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Tag(rev) => Some(rev),
            Self::Branch(rev) => Some(rev),
            Self::ShortCommit(rev) => Some(rev),
            Self::BranchOrTag(rev) => Some(rev),
            Self::BranchOrTagOrCommit(rev) => Some(rev),
            Self::FullCommit(rev) => Some(rev),
            Self::NamedRef(rev) => Some(rev),
            Self::DefaultBranch => None,
        }
    }

    /// Converts the [`GitReference`] to a `str` that can be used as a revision.
    pub(crate) fn as_rev(&self) -> &str {
        match self {
            Self::Tag(rev) => rev,
            Self::Branch(rev) => rev,
            Self::ShortCommit(rev) => rev,
            Self::BranchOrTag(rev) => rev,
            Self::BranchOrTagOrCommit(rev) => rev,
            Self::FullCommit(rev) => rev,
            Self::NamedRef(rev) => rev,
            Self::DefaultBranch => "HEAD",
        }
    }

    /// Returns the precise [`GitSha`] of this reference, if it's a full commit.
    pub(crate) fn as_sha(&self) -> Option<GitSha> {
        if let Self::FullCommit(rev) = self {
            Some(GitSha::from_str(rev).expect("Full commit should be exactly 40 characters"))
        } else {
            None
        }
    }
    /// Resolves self to an object ID with objects the `repo` currently has.
    pub(crate) fn resolve(&self, repo: &GitRepository) -> Result<GitOid, GitError> {
        match self {
            // Resolve the commit pointed to by the tag.
            //
            // `^0` recursively peels away from the revision to the underlying commit object.
            // This also verifies that the tag indeed refers to a commit.
            Self::Tag(s) => repo.rev_parse(&format!("refs/remotes/origin/tags/{s}^0")),

            // Resolve the commit pointed to by the branch.
            Self::Branch(s) => repo.rev_parse(&format!("origin/{s}^0")),

            // Attempt to resolve the branch, then the tag.
            Self::BranchOrTag(s) => repo
                .rev_parse(&format!("origin/{s}^0"))
                .or_else(|_| repo.rev_parse(&format!("refs/remotes/origin/tags/{s}^0"))),

            // Attempt to resolve the commit, the tag then the branch.
            Self::BranchOrTagOrCommit(s) => repo
                .rev_parse(&format!("{s}^0"))
                .or_else(|_| repo.rev_parse(&format!("refs/remotes/origin/tags/{s}^0")))
                .or_else(|_| repo.rev_parse(&format!("origin/{s}^0"))),

            // We'll be using the HEAD commit.
            Self::DefaultBranch => repo.rev_parse("refs/remotes/origin/HEAD"),

            // Resolve a direct commit reference.
            Self::FullCommit(s) | Self::ShortCommit(s) | Self::NamedRef(s) => {
                repo.rev_parse(&format!("{s}^0"))
            }
        }
    }

    /// Whether a `rev` looks like a commit hash (ASCII hex digits).
    pub fn looks_like_commit_hash(rev: &str) -> bool {
        rev.len() >= 7 && rev.chars().all(|ch| ch.is_ascii_hexdigit())
    }

    /// Whether a `rev` looks like a commit hash (ASCII hex digits).
    pub fn looks_like_full_commit_hash(rev: &str) -> bool {
        rev.len() == 40 && rev.chars().all(|ch| ch.is_ascii_hexdigit())
    }

    pub fn is_default(&self) -> bool {
        matches!(self, Self::DefaultBranch)
    }
}

impl Display for GitReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str().unwrap_or("HEAD"))
    }
}

/// A remote repository. It gets cloned into a local [`GitDatabase`].
#[derive(PartialEq, Clone, Debug)]
pub(crate) struct GitRemote {
    /// URL to a remote repository.
    url: Url,
}

impl GitRemote {
    /// Creates an instance for a remote repository URL.
    pub(crate) fn new(url: &Url) -> Self {
        Self { url: url.clone() }
    }

    /// Fetches and checkouts to a reference or a revision from this remote
    /// into a local path.
    ///
    /// This ensures that it gets the up-to-date commit when a named reference
    /// is given (tag, branch, refs/*). Thus, network connection is involved.
    ///
    /// When `locked_rev` is provided, it takes precedence over `reference`.
    ///
    /// If we have a previous instance of [`GitDatabase`] then fetch into that
    /// if we can. If that can successfully load our revision then we've
    /// populated the database with the latest version of `reference`, so
    /// return that database and the rev we resolve to.
    pub(crate) fn checkout(
        &self,
        into: &Path,
        db: Option<GitDatabase>,
        reference: &GitReference,
        locked_rev: Option<GitOid>,
        client: &ClientWithMiddleware,
    ) -> Result<(GitDatabase, GitOid), GitError> {
        let locked_ref = locked_rev.map(|oid| GitReference::FullCommit(oid.to_string()));
        let reference = locked_ref.as_ref().unwrap_or(reference);
        if let Some(mut db) = db {
            fetch(&mut db.repo, self.url.as_str(), reference, client)?;

            let resolved_commit_hash = match locked_rev {
                Some(rev) => db.contains(rev).then_some(rev),
                None => reference.resolve(&db.repo).ok(),
            };

            if let Some(rev) = resolved_commit_hash {
                return Ok((db, rev));
            }
        }

        // Otherwise start from scratch to handle corrupt git repositories.
        // After our fetch (which is interpreted as a clone now) we do the same
        // resolution to figure out what we cloned.
        if into.exists() {
            fs_err::remove_dir_all(into)?;
        }

        fs_err::create_dir_all(into)?;
        let mut repo = GitRepository::init(into)?;
        fetch(&mut repo, self.url.as_str(), reference, client)?;
        let rev = match locked_rev {
            Some(rev) => rev,
            None => reference.resolve(&repo)?,
        };

        Ok((GitDatabase { repo }, rev))
    }

    /// Creates a [`GitDatabase`] of this remote at `db_path`.
    #[allow(clippy::unused_self)]
    pub(crate) fn db_at(&self, db_path: &Path) -> Result<GitDatabase, GitError> {
        let repo = GitRepository::open(db_path)?;
        Ok(GitDatabase { repo })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }
}

/// A local clone of a remote repository's database. Multiple [`GitCheckout`]s
/// can be cloned from a single [`GitDatabase`].
pub(crate) struct GitDatabase {
    /// Underlying Git repository instance for this database.
    repo: GitRepository,
}

impl GitDatabase {
    /// Checkouts to a revision at `destination` from this database.
    pub(crate) fn copy_to(&self, rev: GitOid, destination: &Path) -> Result<GitCheckout, GitError> {
        // If the existing checkout exists, and it is fresh, use it.
        // A non-fresh checkout can happen if the checkout operation was
        // interrupted. In that case, the checkout gets deleted and a new
        // clone is created.
        let checkout = match GitRepository::open(destination)
            .ok()
            .map(|repo| GitCheckout::new(rev, repo))
            .filter(GitCheckout::is_fresh)
        {
            Some(co) => co,
            None => GitCheckout::clone_into(destination, self, rev)?,
        };
        Ok(checkout)
    }

    /// Get a short OID for a `revision`, usually 7 chars or more if ambiguous.
    pub(crate) fn to_short_id(&self, revision: GitOid) -> Result<String, GitError> {
        let output = Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("rev-parse")
            .arg("--short")
            .arg(revision.as_str())
            .current_dir(&self.repo.path)
            .output()?;

        let mut result = String::from_utf8(output.stdout)?;

        result.truncate(result.trim_end().len());
        tracing::debug!("result of short id is  {:?}", result);
        Ok(result)
    }

    /// Checks if `oid` resolves to a commit in this database.
    pub(crate) fn contains(&self, oid: GitOid) -> bool {
        self.repo.rev_parse(&format!("{oid}^0")).is_ok()
    }
}

/// A local Git repository.
pub(crate) struct GitRepository {
    /// Path to the underlying Git repository on the local filesystem.
    path: PathBuf,
}

impl GitRepository {
    /// Opens an existing Git repository at `path`.
    pub(crate) fn open(path: &Path) -> Result<GitRepository, GitError> {
        // Make sure there is a Git repository at the specified path.
        Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("rev-parse")
            .current_dir(path)
            .output()?;

        Ok(GitRepository {
            path: path.to_path_buf(),
        })
    }

    /// Initializes a Git repository at `path`.
    fn init(path: &Path) -> Result<GitRepository, GitError> {
        // Initialize the repository.
        Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("init")
            .current_dir(path)
            .output()?;

        Ok(GitRepository {
            path: path.to_path_buf(),
        })
    }

    /// Parses the object ID of the given `refname`.
    fn rev_parse(&self, refname: &str) -> Result<GitOid, GitError> {
        let result = Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("rev-parse")
            .arg(refname)
            .current_dir(&self.path)
            .output()?;

        let mut result = String::from_utf8(result.stdout)?;

        result.truncate(result.trim_end().len());
        result.parse().map_err(GitError::OidParse)
    }
}

/// A local checkout of a particular revision from a [`GitRepository`].
pub(crate) struct GitCheckout {
    /// The git revision this checkout is for.
    revision: GitOid,
    /// Underlying Git repository instance for this checkout.
    repo: GitRepository,
}

impl GitCheckout {
    /// Creates an instance of [`GitCheckout`]. This doesn't imply the checkout
    /// is done. Use [`GitCheckout::is_fresh`] to check.
    ///
    /// * The `repo` will be the checked out Git repository.
    fn new(revision: GitOid, repo: GitRepository) -> Self {
        Self { revision, repo }
    }

    /// Clone a repo for a `revision` into a local path from a `database`.
    /// This is a filesystem-to-filesystem clone.
    fn clone_into(into: &Path, database: &GitDatabase, revision: GitOid) -> Result<Self, GitError> {
        tracing::debug!("cloning into {:?} from {:?}", database.repo.path, into);
        let dirname = into.parent().expect("into path must have a parent");
        fs_err::create_dir_all(dirname)?;
        if into.exists() {
            fs_err::remove_dir_all(into)?;
        }

        // Perform a local clone of the repository, which will attempt to use
        // hardlinks to set up the repository. This should speed up the clone operation
        // quite a bit if it works.
        let output = Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("clone")
            .arg("--local")
            // Make sure to pass the local file path and not a file://... url. If given a url,
            // Git treats the repository as a remote origin and gets confused because we don't
            // have a HEAD checked out.
            .arg(dunce::simplified(&database.repo.path).display().to_string())
            .arg(dunce::simplified(into).display().to_string())
            .output()?;

        tracing::debug!("output after cloning {:?}", output);

        let repo = GitRepository::open(into)?;
        let checkout = GitCheckout::new(revision, repo);
        checkout.reset()?;
        Ok(checkout)
    }

    /// Checks if the `HEAD` of this checkout points to the expected revision.
    fn is_fresh(&self) -> bool {
        match self.repo.rev_parse("HEAD") {
            Ok(id) if id == self.revision => {
                // See comments in reset() for why we check this
                self.repo.path.join(CHECKOUT_READY_LOCK).exists()
            }
            _ => false,
        }
    }

    /// This performs `git reset --hard` to the revision of this checkout, with
    /// additional interrupt protection by a dummy file [`CHECKOUT_READY_LOCK`].
    ///
    /// If we're interrupted while performing a `git reset` (e.g., we die
    /// because of a signal) Cargo needs to be sure to try to check out this
    /// repo again on the next go-round.
    ///
    /// To enable this we have a dummy file in our checkout, [`.ok`],
    /// which if present means that the repo has been successfully reset and is
    /// ready to go. Hence if we start to do a reset, we make sure this file
    /// *doesn't* exist, and then once we're done we create the file.
    ///
    /// [`.ok`]: CHECKOUT_READY_LOCK
    fn reset(&self) -> Result<(), GitError> {
        let ok_file = self.repo.path.join(CHECKOUT_READY_LOCK);
        let _ = fs_err::remove_file(&ok_file);

        tracing::debug!("reset {} to {}", self.repo.path.display(), self.revision);

        // Perform the hard reset.
        Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("reset")
            .arg("--hard")
            .arg(self.revision.as_str())
            .current_dir(&self.repo.path)
            .output()?;

        // Update submodules (`git submodule update --recursive`).
        Command::new(GIT.as_ref().map_err(|e| e.clone())?)
            .arg("submodule")
            .arg("update")
            .arg("--recursive")
            .arg("--init")
            .current_dir(&self.repo.path)
            .output()
            .map(drop)?;

        fs_err::File::create(ok_file)?;
        Ok(())
    }
}

/// Attempts to fetch the given git `reference` for a Git repository.
///
/// This is the main entry for git clone/fetch. It does the following:
///
/// * Turns [`GitReference`] into refspecs accordingly.
/// * Dispatches `git fetch` using the git CLI.
///
/// The `remote_url` argument is the git remote URL where we want to fetch from.
pub(crate) fn fetch(
    repo: &mut GitRepository,
    remote_url: &str,
    reference: &GitReference,
    client: &ClientWithMiddleware,
) -> Result<(), GitError> {
    let oid_to_fetch = match github_fast_path(repo, remote_url, reference, client) {
        Ok(FastPathRev::UpToDate) => return Ok(()),
        Ok(FastPathRev::NeedsFetch(rev)) => Some(rev),
        Ok(FastPathRev::Indeterminate) => None,
        Err(e) => {
            tracing::debug!("failed to check github fast path {:?}", e);
            None
        }
    };

    // Translate the reference desired here into an actual list of refspecs
    // which need to get fetched. Additionally record if we're fetching tags.
    let mut refspecs = Vec::new();
    let mut tags = false;
    let mut refspec_strategy = RefspecStrategy::All;
    // The `+` symbol on the refspec means to allow a forced (fast-forward)
    // update which is needed if there is ever a force push that requires a
    // fast-forward.
    match reference {
        // For branches and tags we can fetch simply one reference and copy it
        // locally, no need to fetch other branches/tags.
        GitReference::Branch(branch) => {
            refspecs.push(format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"));
        }

        GitReference::Tag(tag) => {
            refspecs.push(format!("+refs/tags/{tag}:refs/remotes/origin/tags/{tag}"));
        }

        GitReference::BranchOrTag(branch_or_tag) => {
            refspecs.push(format!(
                "+refs/heads/{branch_or_tag}:refs/remotes/origin/{branch_or_tag}"
            ));
            refspecs.push(format!(
                "+refs/tags/{branch_or_tag}:refs/remotes/origin/tags/{branch_or_tag}"
            ));
            refspec_strategy = RefspecStrategy::First;
        }

        // For ambiguous references, we can fetch the exact commit (if known); otherwise,
        // we fetch all branches and tags.
        GitReference::ShortCommit(branch_or_tag_or_commit)
        | GitReference::BranchOrTagOrCommit(branch_or_tag_or_commit) => {
            // The `oid_to_fetch` is the exact commit we want to fetch. But it could be the exact
            // commit of a branch or tag. We should only fetch it directly if it's the exact commit
            // of a short commit hash.
            if let Some(oid_to_fetch) =
                oid_to_fetch.filter(|oid| is_short_hash_of(branch_or_tag_or_commit, *oid))
            {
                refspecs.push(format!("+{oid_to_fetch}:refs/commit/{oid_to_fetch}"));
            } else {
                // We don't know what the rev will point to. To handle this
                // situation we fetch all branches and tags, and then we pray
                // it's somewhere in there.
                refspecs.push(String::from("+refs/heads/*:refs/remotes/origin/*"));
                refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
                tags = true;
            }
        }

        GitReference::DefaultBranch => {
            refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
        }

        GitReference::NamedRef(rev) => {
            refspecs.push(format!("+{rev}:{rev}"));
        }

        GitReference::FullCommit(rev) => {
            if let Some(oid_to_fetch) = oid_to_fetch {
                refspecs.push(format!("+{oid_to_fetch}:refs/commit/{oid_to_fetch}"));
            } else {
                // There is a specific commit to fetch and we will do so in shallow-mode only
                // to not disturb the previous logic.
                // Note that with typical settings for shallowing, we will just fetch a single `rev`
                // as single commit.
                // The reason we write to `refs/remotes/origin/HEAD` is that it's of special significance
                // when during `GitReference::resolve()`, but otherwise it shouldn't matter.
                refspecs.push(format!("+{rev}:refs/remotes/origin/HEAD"));
            }
        }
    }

    tracing::debug!(
        "Performing a Git fetch for: {remote_url} with repo path {}",
        repo.path.display()
    );
    let result = match refspec_strategy {
        RefspecStrategy::All => fetch_with_cli(repo, remote_url, refspecs.as_slice(), tags),
        RefspecStrategy::First => {
            // Try each refspec
            let mut errors = refspecs
                .iter()
                .map_while(|refspec| {
                    let fetch_result =
                        fetch_with_cli(repo, remote_url, std::slice::from_ref(refspec), tags);

                    // Stop after the first success and log failures
                    match fetch_result {
                        Err(ref err) => {
                            tracing::debug!("failed to fetch refspec `{refspec}`: {err}");
                            Some(fetch_result)
                        }
                        Ok(()) => None,
                    }
                })
                .collect::<Vec<_>>();

            if errors.len() == refspecs.len() {
                if let Some(result) = errors.pop() {
                    // Use the last error for the message
                    result
                } else {
                    // Can only occur if there were no refspecs to fetch
                    Ok(())
                }
            } else {
                Ok(())
            }
        }
    };
    tracing::debug!("fetched with cli {:?}", result);
    result
}

/// Attempts to use `git` CLI installed on the system to fetch a repository.
fn fetch_with_cli(
    repo: &mut GitRepository,
    url: &str,
    refspecs: &[String],
    tags: bool,
) -> Result<(), GitError> {
    let mut cmd = Command::new(GIT.as_ref().map_err(|err| err.clone())?);
    cmd.arg("fetch");
    if tags {
        cmd.arg("--tags");
    }
    cmd.arg("--force") // handle force pushes
        .arg("--update-head-ok") // see discussion in #2078
        .arg(url)
        .args(refspecs)
        //     // If cargo is run by git (for example, the `exec` command in `git
        //     // rebase`), the GIT_DIR is set by git and will point to the wrong
        //     // location (this takes precedence over the cwd). Make sure this is
        //     // unset so git will look at cwd for the repo.
        .env_remove(GIT_DIR)
        .current_dir(&repo.path);

    // // We capture the output to avoid streaming it to the user's console during clones.
    // // The required `on...line` callbacks currently do nothing.
    // // The output appears to be included in error messages by default.
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8(output.stderr)?;
        return Err(GitError::Fetch(url.to_string(), stderr));
    }
    tracing::debug!("git fetch output: {:?}", output);
    Ok(())
}

/// The result of GitHub fast path check. See [`github_fast_path`] for more.
enum FastPathRev {
    /// The local rev (determined by `reference.resolve(repo)`) is already up to
    /// date with what this rev resolves to on GitHub's server.
    UpToDate,
    /// The following SHA must be fetched in order for the local rev to become
    /// up-to-date.
    NeedsFetch(GitOid),
    /// Don't know whether local rev is up-to-date. We'll fetch _all_ branches
    /// and tags from the server and see what happens.
    Indeterminate,
}

/// Attempts GitHub's special fast path for testing if we've already got an
/// up-to-date copy of the repository.
///
/// Updating the index is done pretty regularly so we want it to be as fast as
/// possible. For registries hosted on GitHub (like the crates.io index) there's
/// a fast path available to use[^1] to tell us that there's no updates to be
/// made.
///
/// Note that this function should never cause an actual failure because it's
/// just a fast path. As a result, a caller should ignore `Err` returned from
/// this function and move forward on the normal path.
///
/// [^1]: <https://developer.github.com/v3/repos/commits/#get-the-sha-1-of-a-commit-reference>
fn github_fast_path(
    repo: &mut GitRepository,
    url: &str,
    reference: &GitReference,
    client: &ClientWithMiddleware,
) -> Result<FastPathRev, GitError> {
    let url = Url::parse(url)?;
    if !is_github(&url) {
        return Ok(FastPathRev::Indeterminate);
    }

    let local_object = reference.resolve(repo).ok();
    let github_branch_name = match reference {
        GitReference::Branch(branch) => branch,
        GitReference::Tag(tag) => tag,
        GitReference::BranchOrTag(branch_or_tag) => branch_or_tag,
        GitReference::DefaultBranch => "HEAD",
        GitReference::NamedRef(rev) => rev,
        GitReference::FullCommit(rev)
        | GitReference::ShortCommit(rev)
        | GitReference::BranchOrTagOrCommit(rev) => {
            // `revparse_single` (used by `resolve`) is the only way to turn
            // short hash -> long hash, but it also parses other things,
            // like branch and tag names, which might coincidentally be
            // valid hex.
            //
            // We only return early if `rev` is a prefix of the object found
            // by `revparse_single`. Don't bother talking to GitHub in that
            // case, since commit hashes are permanent. If a commit with the
            // requested hash is already present in the local clone, its
            // contents must be the same as what is on the server for that
            // hash.
            //
            // If `rev` is not found locally by `revparse_single`, we'll
            // need GitHub to resolve it and get a hash. If `rev` is found
            // but is not a short hash of the found object, it's probably a
            // branch and we also need to get a hash from GitHub, in case
            // the branch has moved.
            if let Some(ref local_object) = local_object {
                if is_short_hash_of(rev, *local_object) {
                    return Ok(FastPathRev::UpToDate);
                }
            }
            rev
        }
    };

    // This expects GitHub urls in the form `github.com/user/repo` and nothing
    // else
    let mut pieces = url.path_segments().ok_or_else(|| {
        GitError::GitUrlFormat(
            url.as_str().to_string(),
            "no path segments on url".to_string(),
        )
    })?;
    let username = pieces.next().ok_or_else(|| {
        GitError::GitUrlFormat(
            url.as_str().to_string(),
            "couldn't find username or organisation name".to_string(),
        )
    })?;
    let repository = pieces.next().ok_or_else(|| {
        GitError::GitUrlFormat(
            url.as_str().to_string(),
            "couldn't find repository name".to_string(),
        )
    })?;
    if pieces.next().is_some() {
        return Err(GitError::GitUrlFormat(
            url.as_str().to_string(),
            "too many segments in the url".to_string(),
        ));
    }

    // Trim off the `.git` from the repository, if present, since that's
    // optional for GitHub and won't work when we try to use the API as well.
    let repository = repository.strip_suffix(".git").unwrap_or(repository);

    let url = format!(
        "https://api.github.com/repos/{username}/{repository}/commits/{github_branch_name}"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        tracing::debug!("Attempting GitHub fast path for: {url}");
        let mut request = client.get(&url);
        request = request.header("Accept", "application/vnd.github.3.sha");
        request = request.header("User-Agent", "pixi");
        if let Some(local_object) = local_object {
            request = request.header("If-None-Match", local_object.to_string());
        }

        let response = request.send().await?;
        response.error_for_status_ref()?;
        let response_code = response.status();
        if response_code == StatusCode::NOT_MODIFIED {
            Ok(FastPathRev::UpToDate)
        } else if response_code == StatusCode::OK {
            let oid_to_fetch = response.text().await?.parse()?;
            Ok(FastPathRev::NeedsFetch(oid_to_fetch))
        } else {
            // Usually response_code == 404 if the repository does not exist, and
            // response_code == 422 if exists but GitHub is unable to resolve the
            // requested rev.
            Ok(FastPathRev::Indeterminate)
        }
    })
}

/// Whether a `url` is one from GitHub.
fn is_github(url: &Url) -> bool {
    url.host_str() == Some("github.com")
}

/// Whether `rev` is a shorter hash of `oid`.
fn is_short_hash_of(rev: &str, oid: GitOid) -> bool {
    let long_hash = oid.to_string();
    match long_hash.get(..rev.len()) {
        Some(truncated_long_hash) => truncated_long_hash.eq_ignore_ascii_case(rev),
        None => false,
    }
}
