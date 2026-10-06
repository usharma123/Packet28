# Explicit command reduction

Native hooks preserve the host's command and permission handling. Request reduced output explicitly, for example with `Packet28 gh pr view 71 --repo owner/repo`.

A successful PR view summarizes its number, state, author, and title, retains the identifying URL, and shows a body preview of at most 320 UTF-8 bytes and eight lines. Repeated header fields and decorative images are omitted. A visible marker identifies omitted body or additional metadata. If the standard header separator is absent or malformed, the reducer treats the output conservatively as preview text rather than stripping an assumed header. Failed PR reads retain the complete partial stdout and stderr diagnostics in the preview and preserve the command's exit code; the body limit applies to successful reads.

For full current output, rerun the same original `gh pr view` command with its PR and repository arguments. This requests a fresh read. The direct `Packet28 gh` wrapper does not persist a raw artifact. Benchmark estimates count the complete visible CLI output; a shorter preview is not a claim about total model or provider token usage.
