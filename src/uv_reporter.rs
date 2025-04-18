use indicatif::ProgressBar;
use itertools::Itertools;
use pixi_progress::{self, ProgressBarMessageFormatter, ScopedTask};
use std::{collections::HashMap, sync::Arc, time::Duration};
use uv_distribution_types::{BuildableSource, CachedDist, Name, VersionOrUrlRef};
use uv_normalize::PackageName;

fn create_progress(length: u64, message: &'static str) -> ProgressBar {
    // Construct a progress bar to provide some indication on what is currently downloading.
    //  For instance if we could also show at what speed the downloads are progressing or the total
    //  size of the downloads that would really help the user I think.
    let pb = pixi_progress::global_multi_progress().add(ProgressBar::new(length));
    pb.set_style(pixi_progress::default_progress_style());
    pb.set_prefix(message);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

pub struct UvReporterOptions {
    length: Option<u64>,
    top_level_message: &'static str,
    progress_bar: Option<ProgressBar>,
    starting_tasks: Vec<String>,
}

impl UvReporterOptions {
    pub(crate) fn new() -> Self {
        Self {
            length: None,
            top_level_message: "",
            progress_bar: None,
            starting_tasks: Vec::new(),
        }
    }

    pub(crate) fn with_length(mut self, length: u64) -> Self {
        self.length = Some(length);
        self
    }

    pub(crate) fn with_top_level_message(mut self, message: &'static str) -> Self {
        self.top_level_message = message;
        self
    }

    pub(crate) fn with_existing(mut self, progress_bar: ProgressBar) -> Self {
        self.progress_bar = Some(progress_bar);
        self
    }

    pub(crate) fn with_starting_tasks(mut self, tasks: impl Iterator<Item = String>) -> Self {
        self.starting_tasks = tasks.collect_vec();
        self
    }
}

/// Reports on download progress.
pub struct UvReporter {
    pb: ProgressBar,
    fmt: ProgressBarMessageFormatter,
    scoped_tasks: Arc<std::sync::Mutex<Vec<Option<ScopedTask>>>>,
    name_to_id: HashMap<String, usize>,
}

impl UvReporter {
    /// Create a new instance that will report on the progress the given uv reporter
    /// This uses a set size and message
    pub(crate) fn new(options: UvReporterOptions) -> Self {
        // Use a new progress bar if none was provided.
        let pb = if let Some(pb) = options.progress_bar {
            pb
        } else {
            create_progress(
                options.length.unwrap_or_default(),
                options.top_level_message,
            )
        };

        // Create the formatter
        let fmt = ProgressBarMessageFormatter::new(pb.clone());

        let mut name_to_id = std::collections::HashMap::new();
        let mut starting_tasks = vec![];
        // Add the starting tasks
        for task in options.starting_tasks {
            let scoped_task = fmt.start(task.clone());
            starting_tasks.push(Some(scoped_task));
            name_to_id.insert(task, starting_tasks.len() - 1);
        }

        Self {
            pb,
            fmt,
            scoped_tasks: Arc::new(std::sync::Mutex::new(starting_tasks)),
            name_to_id,
        }
    }

    pub(crate) fn new_arc(options: UvReporterOptions) -> Arc<Self> {
        Arc::new(Self::new(options))
    }

    fn lock(&self) -> std::sync::MutexGuard<Vec<Option<ScopedTask>>> {
        self.scoped_tasks.lock().expect("progress lock poison")
    }

    pub(crate) fn start(&self, message: String) -> usize {
        let task = self.fmt.start(message);
        let mut lock = self.lock();
        lock.push(Some(task));
        lock.len() - 1
    }

    pub(crate) fn finish(&self, id: usize) {
        let mut lock = self.lock();
        let len = lock.len();
        let task = lock
            .get_mut(id)
            .unwrap_or_else(|| panic!("progress bar error idx ({id}) > {len}"))
            .take();
        if let Some(task) = task {
            task.finish();
        }
    }

    pub(crate) fn finish_all(&self) {
        self.pb.finish_and_clear()
    }

    pub(crate) fn increment_progress(&self) {
        self.pb.inc(1);
    }
}

impl uv_installer::PrepareReporter for UvReporter {
    fn on_progress(&self, dist: &CachedDist) {
        if let Some(id) = self.name_to_id.get(&format!("{}", dist.name())) {
            self.finish(*id);
        }
        self.increment_progress();
    }

    fn on_complete(&self) {
        self.finish_all();
    }

    fn on_build_start(&self, dist: &BuildableSource) -> usize {
        let name: String = if let Some(name) = dist.name() {
            name.to_string()
        } else {
            dist.to_string()
        };
        self.start(format!("building {}", name))
    }

    fn on_build_complete(&self, _dist: &BuildableSource, id: usize) {
        self.finish(id);
    }

    fn on_checkout_start(&self, url: &url::Url, _rev: &str) -> usize {
        self.start(format!("cloning {}", url))
    }

    fn on_checkout_complete(&self, _url: &url::Url, _rev: &str, index: usize) {
        self.finish(index);
    }

    // TODO: figure out how to display this nicely
    fn on_download_start(&self, name: &PackageName, _size: Option<u64>) -> usize {
        self.start(format!("downloading {}", name))
    }

    fn on_download_progress(&self, _index: usize, _bytes: u64) {}

    fn on_download_complete(&self, _name: &PackageName, id: usize) {
        self.finish(id);
    }
}

impl uv_installer::InstallReporter for UvReporter {
    fn on_install_progress(&self, wheel: &CachedDist) {
        if let Some(id) = self.name_to_id.get(&format!("{}", wheel.name())) {
            self.finish(*id);
        }
        self.increment_progress();
    }

    fn on_install_complete(&self) {
        self.finish_all()
    }
}

impl uv_resolver::ResolverReporter for UvReporter {
    fn on_progress(&self, name: &PackageName, version: &VersionOrUrlRef) {
        self.pb
            .set_message(format!("resolving {}{}", name, version));
    }

    fn on_build_start(&self, dist: &BuildableSource) -> usize {
        self.start(format!("building {}", dist,))
    }

    fn on_build_complete(&self, _dist: &BuildableSource, id: usize) {
        self.finish(id);
    }

    fn on_checkout_start(&self, url: &url::Url, _rev: &str) -> usize {
        self.start(format!("cloning {}", url))
    }

    fn on_checkout_complete(&self, _url: &url::Url, _rev: &str, index: usize) {
        self.finish(index);
    }

    fn on_complete(&self) {
        self.finish_all()
    }

    // TODO: figure out how to display this nicely
    fn on_download_start(&self, name: &PackageName, _size: Option<u64>) -> usize {
        self.start(format!("downloading {}", name))
    }

    fn on_download_progress(&self, _id: usize, _bytes: u64) {}

    fn on_download_complete(&self, _name: &PackageName, id: usize) {
        self.finish(id);
    }
}

impl uv_distribution::Reporter for UvReporter {
    fn on_build_start(&self, dist: &BuildableSource) -> usize {
        self.start(format!("building {}", dist,))
    }

    fn on_build_complete(&self, _dist: &BuildableSource, id: usize) {
        self.finish(id);
    }

    fn on_checkout_start(&self, url: &url::Url, _rev: &str) -> usize {
        self.start(format!("cloning {}", url))
    }

    fn on_checkout_complete(&self, _url: &url::Url, _rev: &str, index: usize) {
        self.finish(index);
    }

    // TODO: figure out how to display this nicely
    fn on_download_start(&self, name: &PackageName, _size: Option<u64>) -> usize {
        self.start(format!("downloading {}", name))
    }

    fn on_download_progress(&self, _id: usize, _bytes: u64) {}

    fn on_download_complete(&self, _name: &PackageName, id: usize) {
        self.finish(id);
    }
}
