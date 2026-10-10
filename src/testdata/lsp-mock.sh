#!/bin/sh
# A minimal language server for bish's tests. Speaks LSP over stdio:
# reads `Content-Length`-framed JSON-RPC, answers requests, and appends
# every message received to the file named by $1, one per line, for
# tests to assert on what bish sent.
#
# Tests can't depend on an installed language server. This is
# `include_str!`'d from a `#[cfg(test)]` module, so it's in the test
# binary only, not shipped as a user-reachable command.
#
# POSIX sh only. `read` takes one byte at a time from a pipe, so the
# `dd` after it lands on the body.

log="$1"
: >"$log"

# Frames and writes one message. `${#1}` counts characters, which is
# bytes for these ASCII replies.
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
		# The client's answer to the `workspace/applyEdit` sent below. Only
		# now does the command that asked finish: a server works during
		# `workspace/executeCommand`, not in its result.
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
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"capabilities":{"positionEncoding":"utf-32","hoverProvider":true,"definitionProvider":true,"referencesProvider":true,"documentSymbolProvider":true,"completionProvider":{"triggerCharacters":["."]},"documentFormattingProvider":true,"renameProvider":true,"codeActionProvider":true,"executeCommandProvider":{"commands":["mock.run"]},"workspaceSymbolProvider":true,"semanticTokensProvider":{"legend":{"tokenTypes":["keyword","parameter","bishInvented"],"tokenModifiers":["declaration"]},"full":true},"textDocumentSync":{"openClose":true,"change":1,"save":true}}}}'
		;;
	*'"method":"textDocument/didOpen"'*)
		# Remembered so `textDocument/definition` answers about the opened
		# document.
		uri=$(printf '%s' "$body" | sed -n 's/.*"uri":"\([^"]*\)".*/\1/p')
		# Diagnostics are pushed, so this is the chance to send some.
		send '{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"'"$uri"'","diagnostics":[
		 {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},
		  "severity":1,"source":"mock","message":"first problem"},
		 {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":4}},
		  "severity":2,"source":"mock","message":"second problem"}]}}'
		# `$4` set to `die` makes the server exit right after publishing: its
		# findings must not linger.
		[ "$4" = "die" ] && break
		;;
	*'"method":"textDocument/didChange"'*)
		# A second, different publication, one finding where the first had
		# two, while the client's diagnostics pane is open.
		send '{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"'"$uri"'","diagnostics":[
		 {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},
		  "severity":1,"source":"mock","message":"the only problem left"}]}}'
		;;
	*'"method":"textDocument/definition"'*)
		# $2, if given, is a URI in another file, for the cross-file path;
		# otherwise the opened document.
		target=${2:-$uri}
		# Three, so `n`/`N` have somewhere to go; `gd` lands on the first.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"uri":"'"$target"'","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":2,"character":0},"end":{"line":2,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}}}]}'
		;;
	*'"method":"codeAction/resolve"'*)
		# Only the action needing resolving gets an edit. Resolving one that
		# only runs a command returns it unchanged, which the client must
		# refuse rather than silently do nothing.
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
		# Three: one with its edit, one needing resolving, and one that only
		# runs a server command and must be refused with a reason.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"title":"Fix it","kind":"quickfix","edit":{"changes":{"'"$uri"'":[
		   {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"FIXED"}]}}},
		 {"title":"Resolve me","kind":"refactor"},
		 {"title":"Run it","command":{"title":"r","command":"mock.run"}}]}'
		;;
	*'"method":"textDocument/rename"'*)
		# Two files: the opened one, and $2 if given, covering open-buffer and
		# on-disk edits. `$3` set to `resource` also asks for a file rename,
		# which bish must refuse.
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
		# Two edits, ascending as servers send them, on different lines, so
		# applying them in the wrong order would corrupt the result.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"FIRST"},
		 {"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":7}},"newText":"THIRD"}]}'
		;;
	*'"method":"textDocument/completion"'*)
		# A plain label, one whose insertText differs, a snippet, and one
		# sorted ahead by sortText.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"isIncomplete":false,"items":[
		 {"label":"beta","kind":6,"detail":"a variable"},
		 {"label":"alpha","kind":3,"insertText":"alpha()","sortText":"0"},
		 {"label":"gamma","kind":3,"insertText":"gamma(${1:x})","insertTextFormat":2},
		 {"label":"belta","kind":6}]}}'
		;;
	*'"method":"textDocument/semanticTokens/full"'*)
		# Deltas, as the protocol has it: line 0 col 0 for 5 (a keyword),
		# 6 further along for 4 (a parameter), then two lines down for 5, of a
		# type the client has no colour for and must not paint.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"data":[
		 0,0,5,0,0,
		 0,6,4,1,1,
		 2,0,5,2,0]}}'
		;;
	*'"method":"workspace/symbol"'*)
		# The flat `SymbolInformation` shape, the only one a workspace query
		# returns: each names its file.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"name":"Outer","kind":23,"location":{"uri":"'"$uri"'",
		  "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}}}},
		 {"name":"other","kind":12,"location":{"uri":"'"${2:-$uri}"'",
		  "range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}}}]}'
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
		# Three uses in the opened document, for the list pane.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":[
		 {"uri":"'"$uri"'","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}}},
		 {"uri":"'"$uri"'","range":{"start":{"line":2,"character":0},"end":{"line":2,"character":5}}}]}'
		;;
	*'"method":"textDocument/hover"'*)
		# Markdown with a fenced signature, as real servers' hovers usually
		# are.
		send '{"jsonrpc":"2.0","id":'"$id"',"result":{"contents":{"kind":"markdown","value":"```sh\necho [args...]\n```\n\nWrites its arguments."}}}'
		;;
	*'"method":"exit"'*)
		break
		;;
	esac
done
