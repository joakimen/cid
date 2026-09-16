# The README demo, as code. Rendered by `make demo`, against the sandbox in
# demo/fixture.sh — everything below is fictional and `gh` is a stub.
#
# Keep this under ~25 seconds: it is a GIF that autoplays on the project page.

Output "docs/demo.gif"

Set Shell bash
Set FontSize 15
Set Width 1200
# The selector fills the terminal, so extra height is dead space in the GIF.
Set Height 280
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 25ms
Set Framerate 24

# Enter the sandbox off-camera: fake HOME, stub gh, freshly built cid.
Hide
Type "source target/demo-fixture/env.sh"
Enter
Sleep 1s
Show

# --- 1. Jump to a repository -------------------------------------------------
# The prompt names the directory, so the cd shows.
Type 'cd "$(cid repo sel)"'
Enter
Sleep 700ms
Type "bill"
Sleep 800ms
Enter
Sleep 900ms

# --- 2. Check out a branch that only exists on the remote --------------------
Type "cid branch checkout"
Enter
Sleep 800ms

# Three rows down is the remote-only branch, at the bottom of the list.
Down
Sleep 250ms
Down
Sleep 250ms
Down
Sleep 700ms
Enter
Sleep 1300ms

# --- 3. Check out a pull request, reading its checks and body first ----------
Type "cid pr checkout"
Enter
Sleep 1600ms
Down
Sleep 1600ms
Enter
Sleep 1300ms

# --- 4. Find a note by what it says ------------------------------------------
# ctrl-r switches the query from note names to the text inside them.
# $EDITOR is cat in the sandbox, so the note lands on screen.
Type "cid note open"
Enter
Sleep 600ms
Ctrl+R
Type "redis"
Sleep 1500ms
Enter
Sleep 1800ms

# --- 5. Find a file where you are standing and open it -----------------------
Type "cid edit"
Enter
Sleep 600ms
Type "quota"
Sleep 1000ms
Enter
Sleep 1800ms
