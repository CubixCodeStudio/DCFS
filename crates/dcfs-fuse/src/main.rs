//! DCFS FUSE mount binary.
//!
//! ```text
//! dcfs-fuse <mountpoint> [--server URL] [--allow-other] [--read-only]
//! ```

fn main() {
    let mut mountpoint = None;
    let mut mode = std::env::var("DCFS_MODE").unwrap_or_else(|_| "stream".to_string());
    let mut cache_dir = std::env::var("DCFS_CACHE_DIR").ok();
    let mut cache_size = std::env::var("DCFS_CACHE_SIZE").ok();
    let mut server =
        std::env::var("DCFS_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    // Only from the environment: a token on the command line shows up in `ps`.
    let token = std::env::var("DCFS_TOKEN").ok();
    let mut allow_other = false;
    let mut read_only = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => match args.next() {
                Some(url) => server = url,
                None => fail("--server needs a URL"),
            },
            "--mode" => match args.next() {
                Some(value) => mode = value,
                None => fail("--mode needs stream or mirror"),
            },
            "--cache-dir" => match args.next() {
                Some(value) => cache_dir = Some(value),
                None => fail("--cache-dir needs a path"),
            },
            "--cache-size" => match args.next() {
                Some(value) => cache_size = Some(value),
                None => fail("--cache-size needs a byte count"),
            },
            "--allow-other" => allow_other = true,
            "--read-only" | "-r" => read_only = true,
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            other if other.starts_with('-') => fail(&format!("unknown option {other}")),
            other => mountpoint = Some(other.to_string()),
        }
    }

    let Some(mountpoint) = mountpoint else {
        fail("no mountpoint given; try --help");
    };

    let cache_size: u64 = match cache_size.as_deref() {
        None => 1024 * 1024 * 1024,
        Some(raw) => match raw.parse() {
            Ok(0) | Err(_) => fail("--cache-size must be a positive byte count"),
            Ok(value) => value,
        },
    };
    let mode = match mode.as_str() {
        "stream" => dcfs_fuse::Mode::Stream {
            budget_bytes: cache_size,
        },
        "mirror" => dcfs_fuse::Mode::Mirror,
        other => fail(&format!(
            "unknown mode {other:?}: expected stream or mirror"
        )),
    };
    // Default under the user's own runtime/temp directory, not a shared one:
    // the cache holds plaintext file contents.
    let cache_dir = cache_dir.unwrap_or_else(|| {
        std::env::temp_dir()
            .join(format!("dcfs-cache-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    });

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    run(Options {
        mountpoint: &mountpoint,
        server: &server,
        token: token.as_deref(),
        mode,
        cache_dir: &cache_dir,
        allow_other,
        read_only,
    });
}

/// Written at column zero so the help text is not indented by the source.
const HELP: &str = "\
Usage: dcfs-fuse <mountpoint> [options]

Options:
  --server URL      Server to mount. Default $DCFS_SERVER or http://127.0.0.1:8080
  --mode MODE       stream (default) or mirror, see below. $DCFS_MODE
  --cache-dir PATH  Where cached file contents live. $DCFS_CACHE_DIR
  --cache-size N    Stream-mode cache budget in bytes, default 1073741824 (1 GiB).
                    $DCFS_CACHE_SIZE
  --read-only, -r   Mount read-only
  --allow-other     Let other users reach the mount

Modes:
  stream  Files stay on the server. Blocks are fetched as they are read and the
          most recently used ones are kept, up to --cache-size. Local disk use
          is bounded however large the filesystem is.
  mirror  Keep a local copy of everything. The whole tree is pulled down in the
          background after mount and nothing is evicted, so reads are served
          locally. Needs room for the entire filesystem.

The bearer token is read from $DCFS_TOKEN, never from argv.
Cached blocks are plaintext: put --cache-dir somewhere only you can read.
Unmount with: fusermount3 -u <mountpoint>";

/// Everything `run` needs, so neither platform's signature grows a tail of
/// positional booleans.
// Only the Linux `run` reads these; the fallback takes the struct and refuses.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Options<'a> {
    mountpoint: &'a str,
    server: &'a str,
    token: Option<&'a str>,
    mode: dcfs_fuse::Mode,
    cache_dir: &'a str,
    allow_other: bool,
    read_only: bool,
}

fn fail(message: &str) -> ! {
    eprintln!("dcfs-fuse: {message}");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn run(options: Options) {
    use dcfs_fuse::{BlockCache, DiscordFs, Fs, HttpClient, WriteLog};
    use fuser::MountOption;
    use std::sync::Arc;

    let runtime = tokio::runtime::Runtime::new().expect("failed to start the async runtime");

    let cache = match runtime.block_on(BlockCache::open(options.cache_dir, options.mode)) {
        Ok(cache) => Arc::new(cache),
        Err(e) => fail(&format!(
            "cannot open the cache directory {}: {e}",
            options.cache_dir
        )),
    };

    // Writes are acknowledged before they are sent, so they are recorded here
    // first. This directory is deliberately not the block cache's: that one is
    // wiped at mount, and anything unsent has to outlive exactly that.
    let log_dir = std::path::Path::new(options.cache_dir).with_extension("writes");
    let log = match WriteLog::open(&log_dir) {
        Ok(log) => Arc::new(log),
        Err(e) => fail(&format!(
            "cannot open the write log at {}: {e}",
            log_dir.display()
        )),
    };

    // Resolve the root before mounting: a bad URL or a wrong token should fail
    // here, with a readable message, rather than as EIO on the first ls.
    let client = Arc::new(HttpClient::with_token(options.server, options.token));
    let fs = match runtime.block_on(Fs::mount_with(client, Some(cache.clone()), Some(log))) {
        Ok(fs) => Arc::new(fs),
        Err(e) => fail(&format!(
            "cannot reach the server at {}: {e}",
            options.server
        )),
    };

    // Anything a previous run accepted but never sent goes before the mount is
    // visible, so nobody reads a file that is about to change under them.
    match runtime.block_on(fs.recover()) {
        Ok(0) => {}
        Ok(replayed) => tracing::info!("replayed {replayed} writes from a previous run"),
        Err(e) => fail(&format!("cannot replay unsent writes: {e}")),
    }

    let mut mount_options = vec![
        MountOption::FSName("dcfs".to_string()),
        MountOption::Subtype("dcfs".to_string()),
        MountOption::NoAtime,
    ];
    if options.allow_other {
        mount_options.push(MountOption::AllowOther);
    }
    mount_options.push(if options.read_only {
        MountOption::RO
    } else {
        MountOption::RW
    });

    if options.mode.is_mirror() {
        // The mount is usable straight away: anything not mirrored yet is
        // fetched on demand like it would be in stream mode.
        let fs = fs.clone();
        runtime.spawn(async move {
            match fs.prefetch_all().await {
                Ok((files, bytes)) => {
                    tracing::info!("mirror complete: {files} files, {bytes} bytes")
                }
                Err(e) => tracing::warn!("mirror incomplete: {e}"),
            }
        });
    }

    tracing::info!(
        mode = if options.mode.is_mirror() {
            "mirror"
        } else {
            "stream"
        },
        cache = options.cache_dir,
        "mounting {} at {}",
        options.server,
        options.mountpoint
    );
    let adapter = DiscordFs::new(fs, runtime.handle().clone());
    let result = fuser::mount2(adapter, options.mountpoint, &mount_options);

    // The cache is plaintext and process-scoped; do not leave it behind.
    runtime.block_on(cache.discard());

    if let Err(e) = result {
        fail(&format!("mount failed: {e}"));
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_options: Options) -> ! {
    fail("mounting is only supported on Linux with FUSE3 in v0.1");
}
