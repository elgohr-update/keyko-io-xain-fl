with import <nixpkgs> { };

stdenv.mkDerivation rec {
  name = "xain-fl";
  buildInputs = [
    pkgs.latest.rustChannels.nightly.rust
    pkgs.cargo-edit
    pkgs.rustfmt
    pkgs.openssl
    pkgs.pkg-config
    python37Packages.numpy
    python37Packages.ipython
    python37Packages.virtualenv
    docker-compose
    # profiling and visualization
    pkgs.gperftools
    pkgs.graphviz
    pkgs.gv
  ];
  RUST_BACKTRACE = 1;
  src = null;
  shellHook = ''
    # Allow the use of wheels.
    SOURCE_DATE_EPOCH=$(date +%s)

    VENV=.ignore/${name}
    if test ! -d $VENV; then
      virtualenv $VENV
    fi
    source ./$VENV/bin/activate
    # pip install -U -e './python/[dev]'

    export PYTHONPATH=`pwd`/$VENV/${python.sitePackages}/:$PYTHONPATH
    export LD_LIBRARY_PATH=${lib.makeLibraryPath [ stdenv.cc.cc ]}
    export LIBTCMALLOC_PATH=${lib.makeLibraryPath [ pkgs.gperftools ]}/libtcmalloc.so
    
    # This is to profile memory usage in the aggregator. See:
    # https://stackoverflow.com/questions/38254937/how-do-i-debug-a-memory-issue-in-rust
    #
    # First start the aggregator with the tcmalloc allocator:
    #
    #   LD_PRELOAD="$LIBTCMALLOC_PATH" HEAPPROFILE=./profile ./target/debug/aggregator -c configs/dev-aggregator.toml 
    #
    # Then generate SVGs out of the heap profiles generated by gperftools:
    #
    #   pprof --svg ./target/debug/aggregator ./profile.0100.heap profile.0100.heap.svg 

  '';
}
