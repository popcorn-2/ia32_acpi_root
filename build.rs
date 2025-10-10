fn main() {
	println!("cargo:rustc-link-arg=--target=x86_64-unknown-popcorn");
	println!("cargo:rustc-link-arg=--sysroot=/Users/Eliyahu/popcorn2/_build/sysroot");
}
