# mbuild

`mbuild` is an experimental build system with modular builder components.
The project targets reproducible results: builder outputs are hashed and support separate build and materialize operations.
The initial focus is a `source builder` that produces source trees and a `binary builder` that builds artifacts in containers.
