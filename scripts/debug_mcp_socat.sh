#!/bin/sh
exec /opt/homebrew/bin/socat -lf /tmp/p28_socat.log -v -x STDIO EXEC:"/usr/local/lib/node_modules/packet28/node_modules/@packet28/darwin-arm64/bin/Packet28 mcp serve --root /Users/utsavsharma/Documents/GitHub/Coverage",nofork
