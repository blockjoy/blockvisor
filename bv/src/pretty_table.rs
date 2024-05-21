use cli_table::{
    format::Justify,
    format::{Border, HorizontalLine, Separator},
    CellStruct,
    Color::{Blue, Cyan, Green, Red, Yellow},
    ColorChoice, Style, Table, TableStruct, WithTitle,
};

use crate::node_state::NodeStatus;

fn style_node_status(cell: CellStruct, value: &NodeStatus) -> CellStruct {
    match value {
        NodeStatus::Busy => cell.foreground_color(Some(Yellow)),
        NodeStatus::Running => cell.foreground_color(Some(Green)),
        NodeStatus::Stopped => cell.foreground_color(Some(Yellow)),
        NodeStatus::Failed => cell.foreground_color(Some(Red)),
    }
}

#[derive(Debug, Clone, Table)]
pub struct PrettyTableRow {
    #[table(title = "ID", justify = "Justify::Right")]
    pub id: String,
    #[table(title = "Name", color = "Cyan")]
    pub name: String,
    #[table(title = "Image", color = "Blue")]
    pub image: String,
    #[table(title = "Network", color = "Blue")]
    pub network: String,
    #[table(title = "State", customize_fn = "style_node_status")]
    pub status: NodeStatus,
    #[table(title = "IP Address", color = "Yellow")]
    pub ip: String,
    #[table(title = "Uptime (s)")]
    pub uptime: String,
}

/// Converts into a [`cli_table::TableStruct`] table that could be displayed on command line
///
/// We are putting derives on [`crate::node_state::NodeState`] to convert anything that iterates
/// over this type into a CLI table.
///
/// See <https://docs.rs/cli-table/latest/cli_table/#derive-macro>
pub trait PrettyTable {
    fn to_pretty_table(self) -> TableStruct;
}

impl<T> PrettyTable for T
where
    Self: WithTitle,
{
    fn to_pretty_table(self) -> TableStruct {
        // this will build a table w/o title, w/o borders between the rows and columns
        // and w/ horizontal lines before and after the table
        self.with_title()
            .separator(Separator::builder().build())
            .border(
                Border::builder()
                    .bottom(HorizontalLine::default())
                    .top(HorizontalLine::default())
                    .build(),
            )
            .color_choice(ColorChoice::Auto)
    }
}
