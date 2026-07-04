fn main() {
    let mut x: u32 = 42u32;
    let mut y: i32 = 42i32;

    x = x + 1;
    y = y + 1;
    let sum = x as i32 + y;
    println!("{}", x);
    println!("{}", y);
    println!("{}", sum);
}
