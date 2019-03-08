# Major changes between releases

## Changes in version 0.1.1

**STILL UNDER DEVELOPMENT; NOT RELEASED YET.**

* Switched to the hashbrown implementation of Swiss Tables for hash maps,
  which brings an up to 1% performance improvement during Bazel builds
  that use sandboxfs.

## Changes in version 0.1.0

**Released on 2019-02-05.**

This is the first formal release of the sandboxfs project.

**WARNING:** The interaction points with sandboxfs are subject to change at this
point.  In particular, the command-line interface and the data format used to
reconfigure sandboxfs while it's running *will* most certainly change.
