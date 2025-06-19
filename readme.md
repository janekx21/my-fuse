This is a fuse filesystem with just one 5mb file called matrix.out

It default mounts to /run/janek

I did not implement creating and deleting the file

To run it you need a rust environment with the stable toolchain. This can be installed using rustup.
Alternativly use the nix flake (`nix develop`)

Then run the filesystem with
```
mkdir /tmp/mnt
RUST_LOG=error cargo run --release -- /tmp/mnt
```

