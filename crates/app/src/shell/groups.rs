//! The groups panel the channel list opens on Left: the groups as a side panel on the left edge.
//! OK picks one for the list under it; History opens the History screen instead.

use tv_machine::machine::Measure;
use tv_screens::data::{Group, Logos, Navigate, Output, Screen, ScreenMessage, TvData};
use tv_screens::input::{Action, Hold, Key, Press};
use tv_screens::panels::{Edge, Page, PanelKey, Row, SidePanel};
use tv_ui::Painter;

pub struct Groups {
    panel: SidePanel<usize>,
    groups: Vec<Group>,
}

impl Groups {
    /// The groups with `current` focused.
    pub fn open(data: &dyn TvData, current: &Group) -> Self {
        let groups = data.groups();
        let rows = groups
            .iter()
            .enumerate()
            .map(|(i, g)| Row::action(i, g.name()))
            .collect();
        let focus = groups.iter().position(|g| g == current).unwrap_or(0);
        Groups {
            panel: SidePanel::at(
                Edge::Left,
                Page::list(tv_platform::i18n::msg::common_groups(), rows).focused(focus),
            ),
            groups,
        }
    }
}

impl Screen for Groups {
    fn hold(&self, key: Key) -> Hold {
        PanelKey::hold(key)
    }

    fn key(&mut self, key: Key, press: Press, _: &dyn TvData, _: u64) -> Output {
        let Some(key) = PanelKey::from_action(Action { key, press }) else {
            return Output::default();
        };
        let groups = &self.groups;
        self.panel.handle(key, |_, i| match groups.get(i).cloned() {
            Some(Group::History) => Output::close().and(Output::navigate(Navigate::History)),
            Some(group) => Output {
                message: Some(ScreenMessage::SetGroup(group)),
                ..Output::close()
            },
            None => Output::close(),
        })
    }

    fn closing(&self) -> bool {
        self.panel.closing()
    }

    fn gone(&self) -> bool {
        self.panel.gone()
    }

    fn update(&mut self, dt: f32) {
        self.panel.update(dt);
    }

    fn draw(&mut self, p: Painter, _: &dyn Measure, _: &dyn Logos) {
        self.panel.draw(p);
    }
}
