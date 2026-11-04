# Regression corpus

Inputs that crashed, timed out or exhausted memory in a fuzzing run, kept so that every future
run starts from them. `scripts/fuzz.sh` copies this directory into the startup corpus of each
target.

The files are the result of `cargo fuzz cmin` over every artifact uploaded by the failing CI
runs, so they cover the same edges as the originals in a fraction of the space. The first batch
was collected on 2026-11-04 from the runs that led to 0.3.27: 1,044 artifacts reduced to 165
files (5.3 MB).

Refreshing after a new crash:

```sh
# Download the artifacts of the failing run, one directory per run.
gh run download <run-id> -D /tmp/fuzz-artifacts

# Collect and minimize each target.
for t in container decode parallel hevc; do
  rm -rf /tmp/cmin-$t && mkdir -p /tmp/cmin-$t
  find /tmp/fuzz-artifacts -path "*/artifacts/$t/*" -type f -exec cp {} /tmp/cmin-$t/ \;
  cargo +nightly fuzz cmin "$t" /tmp/cmin-$t
  mkdir -p fuzz/regressions/$t
  cp /tmp/cmin-$t/* fuzz/regressions/$t/
done
```

Commit the new files with the fix that makes them pass. cmin keeps a file only when it adds
coverage, so do not rename or deduplicate by hand.
