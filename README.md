# asahi-fix27
Fix macOS 27 bootability flag from linux side.

# Usage:
```sudo ./asahi-fix27```


If you have partitions with VolBootable flag unset, it will list those and ask to be re-run with the `--confirm` flag.
If no such partitions are detected, it will exit with no output.
