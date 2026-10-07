#! /bin/bash

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# real xkbcomp first, so Xorg's keyboard init still succeeds
/usr/bin/xkbcomp.real "$@"
RC=$?
# now paint, as a child of the surviving sh
DISPLAY=:0 /usr/bin/xsetroot -solid navy >/tmp/paint.log 2>&1
echo "PAINT_RC=$?" >> /tmp/paint.log
exit $RC
