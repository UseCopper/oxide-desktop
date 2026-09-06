static POSSIBLE_BACKENDS: &[&str] = &[
    #[cfg(feature = "winit")]
    "--winit : Run oxide-desktop as a X11 or Wayland client using winit.",
    #[cfg(feature = "udev")]
    "--tty-udev : Run oxide-desktop on a tty using udev (requires root if without logind).",
    #[cfg(feature = "x11")]
    "--x11 : Run oxide-desktop as an X11 client.",
];

#[cfg(feature = "profile-with-tracy-mem")]
#[global_allocator]
static GLOBAL: profiling::tracy_client::ProfiledAllocator<std::alloc::System> =
    profiling::tracy_client::ProfiledAllocator::new(std::alloc::System, 10);

fn detect_backend() -> Option<&'static str> {
    #[cfg(feature = "udev")]
    {
        // Check if we're on a TTY (no X11 or Wayland display)
        let is_tty = std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err();
        if is_tty {
            // Check if we have access to DRM devices
            if std::path::Path::new("/dev/dri").exists() {
                return Some("--tty-udev");
            }
        }
    }

    #[cfg(feature = "x11")]
    if std::env::var("DISPLAY").is_ok() {
        return Some("--x11");
    }

    #[cfg(feature = "winit")]
    {
        // winit can run on both X11 and Wayland
        if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
            return Some("--winit");
        }
    }

    None
}

// Allow in this function because of existing usage
#[allow(clippy::uninlined_format_args)]
fn main() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            "oxide_desktop=info,smithay=warn,smithay_drm=warn,winit=warn,wayland_server=warn,xdg_shell=warn,smithay::backend::egl::error=error"
                .parse()
                .unwrap()
        });
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(env_filter)
        .init();

    #[cfg(feature = "profile-with-tracy")]
    profiling::tracy_client::Client::start();

    profiling::register_thread!("Main Thread");

    #[cfg(feature = "profile-with-puffin")]
    let _server = puffin_http::Server::new(&format!("0.0.0.0:{}", puffin_http::DEFAULT_PORT)).unwrap();
    #[cfg(feature = "profile-with-puffin")]
    profiling::puffin::set_scopes_on(true);

    let arg = ::std::env::args().nth(1);

    // Handle --help and -h
    if matches!(arg.as_deref(), Some("--help") | Some("-h")) {
        println!("USAGE: oxide-desktop [--backend]");
        println!();
        println!("If no backend is specified, auto-detection will be attempted.");
        println!("Possible backends are:");
        for b in POSSIBLE_BACKENDS {
            println!("\t{b}");
        }
        return;
    }

    let backend = arg.as_deref().or_else(|| detect_backend());

    match backend {
        #[cfg(feature = "winit")]
        Some("--winit") => {
            tracing::info!("Starting oxide-desktop with winit backend");
            oxide_desktop::winit::run_winit();
        }
        #[cfg(feature = "udev")]
        Some("--tty-udev") => {
            tracing::info!("Starting oxide-desktop on a tty using udev");
            oxide_desktop::udev::run_udev();
        }
        #[cfg(feature = "x11")]
        Some("--x11") => {
            tracing::info!("Starting oxide-desktop with x11 backend");
            oxide_desktop::x11::run_x11();
        }
        Some(other) => {
            tracing::error!("Unknown backend: {}", other);
        }
        None => {
            #[allow(clippy::disallowed_macros)]
            {
                println!("USAGE: oxide-desktop [--backend]");
                println!();
                println!("If no backend is specified, auto-detection will be attempted.");
                println!("Possible backends are:");
                for b in POSSIBLE_BACKENDS {
                    println!("\t{b}");
                }
            }
        }
    }
}
