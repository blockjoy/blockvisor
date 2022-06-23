use cli_table::{
    format::{Border, HorizontalLine, Separator},
    ColorChoice, TableStruct, WithTitle,
};

/// Converts into a [`cli_table::TableStruct`] table that could be displayed on command line
///
/// We are putting derives on [`ContainerData`] to convert anything that iterates
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
