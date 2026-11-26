#[orbic::table("Notes")]
pub struct Note {
    #[pk]
    pub id: u32,
}

#[orbic::table]
pub struct Other {
    #[pk]
    pub id: u32,
}

fn main() {}
