# kyyn-plugin-sweep

Generic file-sweep source plugin for [kyyn](https://github.com/drshade/kyyn):
point it at a directory and glob patterns; every matched file becomes a hashed,
immutable evidence item. Identity is the path relative to the root (filenames
carry information); a changed file is a new version and new curation work.
