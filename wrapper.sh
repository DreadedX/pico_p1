#!/bin/bash
probe-run --chip RP2040 --log-format="{L} {s}
└─ [{t}] {m} @ {F}:{l}" $@
