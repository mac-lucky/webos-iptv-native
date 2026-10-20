//! TiviMate-style screens built on `tv_ui`.

// Shared by every screen: look, icons, remote keys and pointer, time strings, display lists.
pub mod draw;
pub mod icons;
pub mod input;
pub mod number;
pub mod point;
pub mod theme;
pub mod time;

// Wave C screens (declared by the lead; each file is owned by one agent).
pub mod data;
pub mod groups;
pub mod guide;
pub mod library;
pub mod lists;
pub mod panels;
pub mod playback;
pub mod player;
// Movies and Shows (VOD).
pub mod vod;

// Parental controls, the channel name editor and programme reminders.
pub mod names;
pub mod parental;
pub mod reminders;
