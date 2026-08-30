#!/bin/sh
# A minimal language server for bish's own tests.
#
# Speaks real LSP over stdio: reads `Content-Length`-framed JSON-RPC,
# answers `initialize` and `textDocument/hover`, and appends every
# message it received to the file named by $1, one per line, so a test
# can assert on what bish actually sent.
#
# This exists because the tests cannot depend on a real language server
# being installed -- that would be an external dependency in a project
# whose whole point is not having any. It lives in `src/testdata/`
# alongside the other fixtures and is pulled in with `include_str!` from
# inside a `#[cfg(test)]` module, so it is part of the test harness
# binary and of no other build. A `bish tool lsp-mock` subcommand would
# have shipped in the release binary as a user-reachable command that
# exists only to serve tests.
#
# Earlier phases got by with an echo server (`printf <reply>; exec cat`)
# and let bish's own decoder read back what it sent. That stops working
# here: hover is a *request*, so the reply has to depend on what
# arrived, which means actually parsing the framing.
#
# POSIX sh only. `read` consumes one byte at a time from a pipe, which
# is what makes the `dd` that follows it land on the body rather than
# somewhere inside it.

log="$1"
: >"$log"

# Frames and writes one message. `${#1}` counts characters, which is a
# byte count for the ASCII these replies are made of.
send() {
	printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"
}

cr=$(printf '\r')

while :; do
	len=""
	# Headers, up to the blank line. Each arrives with a trailing CR.
	while IFS= read -r line; do
		line=${line%"$cr"}
		[ -z "$line" ] && break
		case "$line" in
		Content-Length:* | content-length:*)
			len=${line#*:}
			len=${len# }
			;;
		esac
	done
	# EOF, or headers with no length: bish has gone away.
	[ -z "$len" ] && break

	body=$(dd bs=1 count="$len" 2>/dev/null)
	printf '%s\n' "$body" >>"$log"

	id=$(printf '%s' "$body" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
	case "$body" in
	*'"method":"initialize"'*)
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"capabilities":{"positionEncoding":"utf-32","hoverProvider":true,"definitionProvider":true,"textDocumentSync":{"openClose":true,"change":1,"save":true}}}}'
		;;
	*'"method":"textDocument/didOpen"'*)
		# Remembered so `textDocument/definition` can answer about
		# the document the client actually opened.
		uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
		;;
	*'"method":"textDocument/definition"'*)
		# $2, when the test gave one, is a URI in another file, so
		# the cross-file path can be exercised; otherwise line 1 of
		# the document the client opened.
		target=${2:-$uri}
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"uri":"'"$target"'","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}}}'
		;;
	*'"method":"textDocument/hover"'*)
		# Markdown with a fenced signature, which is the shape a real
		# server's hover almost always takes.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"contents":{"kind":"markdown","value":"```sh\necho [args...]\n```\n\nWrites its arguments."}}}'
		;;
	*'"method":"exit"'*)
		break
		;;
	esac
done
