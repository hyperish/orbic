#[orbic::table("points")]
pub struct Point(u32, u32);

#[orbic::table("boxes")]
pub struct Boxed<T> {
    #[pk]
    pub id: u32,
    pub item: T,
}

fn main() {}
