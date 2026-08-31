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
	*'"applied"'*)
		# The client's answer to the `workspace/applyEdit` sent below.
		# Only now does the command that asked for it finish -- which
		# is the whole shape this fixture exists to exercise: a server
		# does its work *during* `workspace/executeCommand`, not in
		# its result.
		if [ -n "$cmd_id" ]; then
			send '{"jsonrpc":"2.0","id":'"$cmd_id"',"result":null}'
			cmd_id=""
		fi
		;;
	*'"method":"workspace/executeCommand"'*)
		cmd_id=$id
		send '{"jsonrpc":"2.0","id":9001,"method":"workspace/applyEdit","params":{"label":"mock.run","edit":{"changes":{"'"$uri"'":[
		 {"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":7}},"newText":"COMMANDED"}]}}}}'
		;;
	*'"method":"initialize"'*)
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"capabilities":{"positionEncoding":"utf-32","hoverProvider":true,"definitionProvider":true,"referencesProvider":true,"documentSymbolProvider":true,"completionProvider":{"triggerCharacters":["."]},"documentFormattingProvider":true,"renameProvider":true,"codeActionProvider":true,"executeCommandProvider":{"commands":["mock.run"]},"textDocumentSync":{"openClose":true,"change":1,"save":true}}}}'
		;;
	*'"method":"textDocument/didOpen"'*)
		# Remembered so `textDocument/definition` can answer about
		# the document the client actually opened.
		uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
		;;
	*'"method":"textDocument/definition"'*)
		# $2, when the test gave one, is a URI in another file, so
		# the cross-file path can be exercised; otherwise the
		# document the client opened.
		target=${2:-$uri}
		# Three of them, so `n`/`N` cycling has somewhere to go. The
		# first is the one `gd` itself lands on.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"uri":"'"$target"'","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":2,"character":0},"end":{"line":2,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}}}]}'
		;;
	*'"method":"codeAction/resolve"'*)
		# Only the action that said it needed resolving gets an edit
		# back. A real server does the same: resolving something that
		# only runs a command returns it unchanged, with nothing to
		# apply -- which is the case the client must refuse rather
		# than silently do nothing.
		case "$body" in
		*'"title":"Resolve me"'*)
			send '{"jsonrpc":"2.0","id":'"$id"',"result":{"title":"Resolve me","edit":{"changes":{"'"$uri"'":[
			 {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}},"newText":"RESOLVED"}]}}}}'
			;;
		*)
			send '{"jsonrpc":"2.0","id":'"$id"',"result":{"title":"unchanged"}}'
			;;
		esac
		;;
	*'"method":"textDocument/codeAction"'*)
		# Three, on purpose: one carrying its edit, one that must be
		# resolved, and one that only runs a server command and so
		# must be refused with a reason.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"title":"Fix it","kind":"quickfix","edit":{"changes":{"'"$uri"'":[
		   {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"FIXED"}]}}},
		 {"title":"Resolve me","kind":"refactor"},
		 {"title":"Run it","command":{"title":"r","command":"mock.run"}}]}'
		;;
	*'"method":"textDocument/rename"'*)
		# Two files: the one the client opened, and $2 when the test
		# gave one -- so the open-buffer and on-disk halves are both
		# exercised. `$3` set to `resource` makes it also ask for a
		# file rename, which bish must refuse outright.
		new=$(printf '%s' "$body" | sed -n 's/.*"newName":"\([^"]*\)".*/\1/p')
		extra=""
		[ -n "$2" ] && extra=',
		 {"textDocument":{"uri":"'"$2"'","version":1},
		  "edits":[{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}},"newText":"'"$new"'"}]}'
		[ "$3" = "resource" ] && extra="$extra"',
		 {"kind":"rename","oldUri":"file:///old","newUri":"file:///new"}'
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"documentChanges":[
		 {"textDocument":{"uri":"'"$uri"'","version":1},
		  "edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"'"$new"'"}]}'"$extra"']}}'
		;;
	*'"method":"textDocument/formatting"'*)
		# Two edits, given ascending as a server sends them, on
		# different lines -- so applying them in the wrong order
		# would visibly corrupt the result.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"FIRST"},
		 {"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":7}},"newText":"THIRD"}]}'
		;;
	*'"method":"textDocument/completion"'*)
		# A mix on purpose: a plain label, one whose insertText
		# differs from its label, one that is a snippet, and one
		# ordered ahead of the others by sortText.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"isIncomplete":false,"items":[
		 {"label":"beta","kind":6,"detail":"a variable"},
		 {"label":"alpha","kind":3,"insertText":"alpha()","sortText":"0"},
		 {"label":"gamma","kind":3,"insertText":"gamma(${1:x})","insertTextFormat":2},
		 {"label":"belta","kind":6}]}}'
		;;
	*'"method":"textDocument/documentSymbol"'*)
		# The nested form, so the outline has something to indent.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"name":"Outer","kind":23,
		  "range":{"start":{"line":0,"character":0},"end":{"line":2,"character":0}},
		  "selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},
		  "children":[{"name":"inner","kind":6,
		    "range":{"start":{"line":1,"character":0},"end":{"line":1,"character":9}},
		    "selectionRange":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}}]}]}'
		;;
	*'"method":"textDocument/references"'*)
		# Three uses in the document the client opened, so the list
		# pane has something to show and scroll.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"uri":"'"$uri"'","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":2,"character":0},"end":{"line":2,"character":5}}}]}'
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
