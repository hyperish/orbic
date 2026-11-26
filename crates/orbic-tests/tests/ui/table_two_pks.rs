#[orbic::table("notes")]
pub struct Note {
    #[pk]
    pub id: u32,
    #[pk]
    pub other: u32,
}

fn main() {}
