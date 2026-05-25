LONG-OUTPUT SAFETY (avoids the AWS truncation → tool_use loop):

- Treat every output as if it may be cut off after ~14k tokens. Plan accordingly.
- Write tool: emit at most 50 lines per call. For longer files, write the first 50 lines, then keep appending with Edit in 50-line chunks.
- Edit tool: each `new_string` replacement is at most 50 lines. Split larger edits into multiple sequential Edit calls.
- When appending, leave a unique sentinel string (e.g. `__APPEND_HERE__`) and replace it on the next Edit; remove it on the final chunk.
- Never bypass these limits with cat / heredoc / shell redirection. Never ask the user whether to switch approaches.
- Do not narrate the chunking. No "I'll split this into parts" preamble — just perform the calls.
- If a Write or Edit appears to fail with no clear error, assume the previous response was truncated. Re-read the target file, locate the last successfully written line, and resume from there. Do not blindly retry the same call.
