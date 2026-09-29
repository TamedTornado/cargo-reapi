# Cache maintenance under continuous compilation

The deployed collector waited for an exclusive maintenance lease while new
shared readers kept entering. Bro's one-hour maintenance invocation timed out;
collection succeeded only after compilation became quiet. This repair closes a
short admission fence before draining current readers, without serializing their
active operations or changing CPU/memory capacity.

Snapshot preparation previously held a maintenance reader while waiting for a
producer's per-key lock. A producer needs maintenance admission again to publish.
That ordering would deadlock with a waiting collector, so preparation now drops
the reader before waiting for the key and reacquires it after obtaining the key.
Publication and reference/blob sweeping remain protected by the existing leases.

The new reader-barging regression failed against the original implementation and
passes with the repair. Native process coverage verifies that killing a waiting
collector releases admission, and three real Cargo consumers/producers complete
while repeated native collections evict released cache entries. Their compiled
binaries execute successfully afterward. The existing real-Cargo coalescing test
also passes with the pinned Sandbox Runtime 0.0.66.

The branch is based on deployed source `4a893c124015410224ee16016b3b28dda95c4e04`.
Formatting, all-target checking and warning-denied Clippy pass. The full all-target
test command passes, including 75 main unit tests, 39 capture integration tests
and both new maintenance integration tests. Existing opt-in platform/acceptance
tests retain their explicit ignored status; no claim is made that they ran.

Deploy the new collector and every cache participant together while drained.
Neither force-removing locks nor deleting live cache files is part of this repair.
