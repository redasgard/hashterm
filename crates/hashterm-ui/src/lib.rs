//! GTK application shell: window, tabs, terminal pages.

pub mod app;
pub mod attention;
pub mod chrome;
pub mod csd;
pub mod css;
pub mod groupbar;
pub mod links;
pub mod notice;
pub mod page;
pub mod picker;
pub mod search;
pub mod settings;
pub mod slide;
pub mod tabbar;
pub mod title;
pub mod window;

pub use app::run;

/// "GTK 4.18.6, VTE 0.80.1" for `--doctor` and bug reports.
pub fn toolkit_versions() -> String {
    // SAFETY: plain getters with no preconditions; no GTK/VTE init needed.
    let (a, b, c) = unsafe {
        (
            vte4::ffi::vte_get_major_version(),
            vte4::ffi::vte_get_minor_version(),
            vte4::ffi::vte_get_micro_version(),
        )
    };
    format!(
        "GTK {}.{}.{}, VTE {a}.{b}.{c}",
        gtk4::major_version(),
        gtk4::minor_version(),
        gtk4::micro_version()
    )
}
