import os, sys, glob
CONTEXT_FILES = ["AGENTS.md","SOUL.md","TOOLS.md","IDENTITY.md","HEARTBEAT.md"]

def safe_truncate(s, max_bytes):
    b = s.encode()
    if len(b) <= max_bytes: return s
    end = max_bytes
    while end > 0:
        try:
            return b[:end].decode()
        except UnicodeDecodeError:
            end -= 1
    return ""

def cap_str(s, max_chars):           # prompt_builder::cap_str
    if len(s) <= max_chars: return s
    end = len(s[:max_chars].encode())      # byte index of the max_chars-th char
    return safe_truncate(s, end) + "..."

def build_body(root):                # workspace_context::build_context_section (patched)
    parts = ["- Project: %s (%s)" % (os.path.basename(root.rstrip('/')), "Unknown")]
    if os.path.isdir(os.path.join(root, ".git")): parts.append("- Git repository: yes")
    for name in CONTEXT_FILES:       # HashMap order is arbitrary; CONTEXT_FILES order used here
        p = os.path.join(root, name)
        if not os.path.isfile(p): continue
        if os.path.getsize(p) > 32768: continue
        content = open(p, encoding='utf-8', errors='replace').read()
        preview = safe_truncate(content, 200) + "..." if len(content.encode()) > 200 else content
        parts.append('<file name="%s">\n%s\n</file>' % (name, preview))
    return "\n".join(parts)

bad = 0
for root in sorted(glob.glob("/var/lib/docker/volumes/openfang-staging-data/_data/workspaces/*")):
    body = build_body(root)
    capped = cap_str(body, 1000)                 # prompt_builder line: data_section("workspace_context", &cap_str(ws_ctx,1000))
    section = "<workspace_context>\n%s\n</workspace_context>" % capped.rstrip()
    o, c = section.count("<file name="), section.count("</file>")
    flag = "UNBALANCED" if o != c else "ok"
    if o != c: bad += 1
    print("%-16s body=%4d chars  capped=%4d  <file>=%d  </file>=%d  %s" %
          (os.path.basename(root), len(body), len(capped), o, c, flag))
    if o != c:
        print("      tail of section: ...%s" % section[-120:].replace("\n","\\n"))
print("\nagents with an unclosed <file> tag inside <workspace_context>: %d" % bad)
