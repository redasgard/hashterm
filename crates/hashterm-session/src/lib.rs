//! Resurrect engine: full save-dump / restore of the dedicated tmux server's
//! state (layouts, cwds, scrollback, restartable programs). GTK-free; used both
//! by the GUI and by the headless `hashterm dump` / `hashterm restore-pane`
//! subcommands.

pub mod dump;
pub mod procinfo;
pub mod replay;
pub mod restore;
pub mod schema;
pub mod store;

pub use dump::{DumpOptions, dump_server};
pub use restore::{OnConflict, RestoreOptions, RestoreReport, restore_save};
pub use schema::DumpManifest;
pub use store::{SessionStore, StoreError};
