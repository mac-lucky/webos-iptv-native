//! TiviMate-style screens built on `tv_ui`.

// Shared by every screen: look, icons, remote keys, time strings, display lists.
pub mod draw;
pub mod icons;
pub mod input;
pub mod theme;
pub mod time;

// Wave C screens (declared by the lead; each file is owned by one agent).
pub mod data;
pub mod guide;
pub mod lists;
pub mod panels;
pub mod player;
