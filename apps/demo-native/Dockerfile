# Container baseline: the native demo app, statically linked, alone in an empty image.
# Build context: a directory holding the static binary as `demo-native` (bench/native-baseline.sh prepares it).
FROM scratch
COPY demo-native /demo-native
ENTRYPOINT ["/demo-native", "--listen", "0.0.0.0:8090", "--db", "/data/app.db"]
