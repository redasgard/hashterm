//! GTK application shell: window, tabs, terminal pages.

pub mod app;
pub mod csd;
pub mod css;
pub mod groupbar;
pub mod page;
pub mod picker;
pub mod slide;
pub mod tabbar;
pub mod window;

pub use app::run;
