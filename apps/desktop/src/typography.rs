use iced::{Font, font::Weight};

const FAMILY: &str = "IBM Plex Sans";

pub const BODY: Font = Font::with_name(FAMILY);
/// Small secondary lines (email, reset times): Regular reads too thin there.
pub const MEDIUM: Font = Font {
    weight: Weight::Medium,
    ..BODY
};
pub const EMPHASIS: Font = Font {
    weight: Weight::Semibold,
    ..BODY
};
pub const STRONG: Font = Font {
    weight: Weight::Bold,
    ..BODY
};

// Compact popup typography: keep secondary information readable without
// letting the fixed-size window grow wider or taller.
pub const BODY_SIZE: f32 = 13.0;
pub const CONTROL_SIZE: f32 = 11.0;
pub const ACCOUNT_NAME_SIZE: f32 = 15.0;
pub const METADATA_SIZE: f32 = 11.0;
pub const LABEL_SIZE: f32 = 12.0;
pub const VALUE_SIZE: f32 = 12.0;
pub const PERCENTAGE_SIZE: f32 = 14.0;
pub const RESET_TIME_SIZE: f32 = 11.0;
pub const COMPACT_SIZE: f32 = 10.5;
