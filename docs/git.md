# Git and multiple projects

The **Commit** tool window (`Alt+0`, also `Ctrl+Shift+G`; left dock) works
like PyCharm's with its default "no staging area": a toolbar (Refresh,
Rollback, Show Diff, Expand All, Collapse All, Group By Directory), then a
tree of local changes with a checkbox per file, folder and group:
**Changes** (every tracked change against HEAD), **Unversioned Files** and,
during a merge, **Merge Conflicts**. File names are colored by status
(modified blue, added green, deleted grey and struck through, unversioned
red-brown), with the folder dimmed in the flat list. Commit commits exactly
the checked files (`git commit --only`; checked unversioned files are added
first, and what is staged for other files stays staged); new changes start
checked and your choices persist across refreshes. Folder and group boxes
are tri-state. **Amend** rewrites the last commit with the checked files (or
just the message, which it starts from). The message box grows with the
text; `Ctrl+Enter` commits. **Commit and Push…** commits, then opens the Push
dialog: the outgoing commits and the target (`origin/<branch>`, marked new
when the push sets the upstream), Push or Cancel.

Double-click (or Enter, `Ctrl+D`) opens a file's diff (HEAD ↔ working
tree); `F4` jumps to the file; Space toggles the selected row's checkbox.
The right-click menu has Commit File… (checks only those files), Rollback…,
Show Diff, Jump to Source, Add to VCS and Ignore (unversioned files: adds a
root-anchored `/path` pattern to `.gitignore`), Delete… and Copy Path.
**Rollback** (toolbar, menu, Git > Rollback…) acts on the selection, else
the checked files, after asking: tracked files return to HEAD (index and
working tree), added files leave version control but stay on disk, renames
return to the old name, and unversioned files are deleted.

With `"git": { "staging_area": true }` in `settings.json` (PyCharm's
"Enable staging area") the window shows git's index instead: **Staged** /
**Unstaged** groups with stage (`+`) and unstage (`−`) row buttons and menu
items, and Commit commits what is staged. Hunk staging in diffs works in
both modes. While a merge, rebase, cherry-pick or revert is in progress a
banner says so with Abort (and Continue for a rebase); conflicted files show
under Merge Conflicts, open in the editor, and `✓` (Mark Resolved) stages
them; committing during a merge commits the whole result.

Git runs where the project lives (inside WSL for WSL projects), and the
window refreshes on file changes and on index/ref changes (commits or
checkouts from a terminal).

When git refuses a repository whose folder belongs to another user ("dubious
ownership": a folder on another drive, a shared folder, a folder mounted into
WSL), the Commit window says so and offers **Trust This Folder**, which adds it to
git's global `safe.directory` list where the project lives (what git's own
message and VS Code suggest). Inside a dev container, where the bind-mounted
workspace usually belongs to another user than the container's, the editor
runs its own git commands with `-c safe.directory=*` and leaves the config
as it is.

Diffs show unified or side by side, and changed hunks only or the whole
file; the last choice sticks for new diffs. Diffs of changes in the index
or working tree come from `git diff` itself and act per hunk: "Stage hunk"
and "Discard" (asks first) on unstaged changes, "Unstage hunk" on staged
ones.

**Branches**: the title bar shows the current branch (`⎇ main ▾`, with
`↑n`/`↓n` against its upstream and the operation in progress). Clicking it
(or Git > Branches…) opens PyCharm's branches popup: a search box, then
Update Project…, Commit…, Push…, New Branch…, Checkout Tag or Revision…,
and the **Recent** (from HEAD's reflog), **Local** and **Remote** branches
with the current one starred and ahead/behind counts. Each branch opens a
submenu: Checkout (a remote branch gets a tracking local branch), New
Branch from '…'…, Checkout and Rebase onto '<current>', Compare with
'<current>', Show Diff with Working Tree, Rebase '<current>' onto '…',
Merge '…' into '<current>', Update (tracked branches; a branch that is not
checked out is fast-forwarded), Push…, Rename…, Delete (asks again when the
branch has unmerged commits; remote branches are deleted on the remote).
Merges and rebases that stop on conflicts open the Commit window. New
Branch asks for a name with "Checkout branch".

**Update Project** fetches, then merges the upstream (or rebases onto it
with `"git": { "update_method": "rebase" }`). Git > Fetch is `--all
--prune`, Pull is fast-forward only, and Push… (the Push dialog) sets the
upstream on `origin` (or the only remote) the first time. Git never
prompts: credentials come from your credential helper or SSH agent, and
failures are shown in the Commit window.

**Worktrees…** (Git menu) lists the repository's worktrees; choosing one
opens it in a project tab. Typing a branch name adds a worktree for it
(creating the branch if needed) next to the repository as
`<repository>-<branch>` and opens it; `×` removes one (asks; its branch is
kept).

The **Git** tool window (`Alt+9`, View > Git, Git > Show Git Log; bottom
dock) is PyCharm's Log tab. On the left, the **branches tree**: HEAD (the
current branch), Local, Remote (per remote) and Tags; double-clicking a row
filters the log to it. In the middle, a filter bar — text (subject, author
or the start of a hash, as you type), **Branch ▾**, **User ▾** (me and the
authors seen), **Date ▾** (last 24 hours … 12 months), **Paths ▾**
(repository-relative, comma-separated; Git > Show History for Current File
sets it to the active file), `✕` clears them — above the commit graph:
lanes with HEAD's commit ringed, subject, branch/tag labels (HEAD's branch
highlighted, remote branches outlined, tags ◆), author and age.
The log loads 1000 commits at a time and the next page as the list nears
its end, so long histories stay responsive; text, user and date filters
draw the matches as one line (their parents are not listed), while branch
and path filters keep the lanes (`git log --parents` rewrites them).

Click selects a commit, `Ctrl`+click and `Shift`+click (or `Shift+Up/Down`)
select several; `Ctrl+C` copies the selected hashes. On the right, the
selection's **changed files** (for several commits, everything from the
oldest one's parent to the newest one) — double-click opens that diff —
above the **details**: full message, hash, author, date and the branches
containing the commit. The right-click menu has Copy Revision Number,
Create Patch… (`git format-patch` of the selection into a file you
choose), Cherry-Pick (oldest first; merges excluded; already-applied
commits are skipped), Checkout Revision (detached), Compare with Local,
Reset Current Branch to Here… (Soft / Mixed / Hard / Keep), Revert Commit
(newest first; a merge alone, against its first parent), Undo Commit
(HEAD only, unless pushed: a soft reset keeping the changes), Edit Commit
Message… (HEAD; contents and the index are kept), New Branch… and New
Tag…. A cherry-pick or revert that stops on conflicts says which files and
offers Abort; resolve and commit in the Commit window to finish it.

**Compare…** compares the working tree with a branch, tag or any revision
(`HEAD~3`, a commit id). A comparison tab lists the changed files as a tree
or list; its base and target can be changed (the target may be another
revision, and then swapped), and each file opens its diff.

## Stash and shelf

**Git > Stash Changes…** (also in the Commit window's right-click menu, for
the chosen files) asks for an optional message and **Include untracked
files** (preset when the chosen files include unversioned ones), then runs
`git stash push` and reverts those changes. **Git > Unstash Changes…** lists
the stashes (`stash@{n}`, message, branch, age) with the selected one's
files below; double-click a file (or **View Diff**) for its diff (the commit
it was made on ↔ the stash; untracked files against nothing). **Apply** keeps
the stash, **Pop** drops it once applied, **Reinstate index** restores what
was staged as staged, **Drop…** asks first. When the stash conflicts with the
files now, the Commit window shows the files under Merge Conflicts and the
stash is kept.

The **shelf** is PyCharm's: **Shelve Changes…** (the Commit window's
right-click menu, or Git > Shelve Changes… for the selected, else checked,
files) asks for a name, prefilled from the commit message's first line or
the file names, saves the changes as a patch against HEAD
(`.next_code/shelf/<id>/shelf.patch`, binary-safe, plus `info.json` with
the name, date, base commit and files; a `.gitignore` of `*` in
`.next_code/shelf/` keeps them out of git) and rolls the files back
(modified and deleted files return to HEAD, new files are deleted). The
Commit window's **Shelf** tab (next to **Changes**; Git > Show Shelf) lists
shelves newest first with their file count and age; expand one for its
files, double-click a file for its diff. **Unshelve…** (toolbar or
right-click) applies the patch to the working tree, removing the shelf or
keeping it as you choose; if the patch no longer applies cleanly it is
merged three-way and conflicting files show under Merge Conflicts (the
shelf is then kept). **Delete…** asks first. Shelves live under the
repository root where the project lives (WSL, container), and headless
commands use the same code.

Limits: unshelving restores changes as unstaged (staged/unstaged is not
kept), and a three-way unshelve refuses files that have unstaged changes
of their own ("does not match index"): commit, stash or roll them back
first. Shelving needs at least one commit.

## Changes and blame in the editor

**Change markers**: the editor's gutter (between the line numbers and the
fold arrows) marks lines that differ from the last commit — a green bar for
added lines, a blue bar for modified ones, a grey triangle where lines were
deleted — or from git's index with `"git": { "staging_area": true }`.
They follow typing (the diff runs on a background thread 100 ms after you
stop, 0.4–1 s for files over 256 KiB / 2 MiB) and refresh with the Commit
window: after saves, commits, staging, checkouts and changes from a
terminal. Untracked and ignored files have none; a file staged but never
committed is all added. Line endings do not count (a CRLF checkout of an LF
file shows no changes); adding or removing the final line break marks the
last line.

Clicking a marker opens the change: the committed lines (red) above the
current ones (green), with **↑/↓** (previous/next change), **Rollback**
(puts the committed lines back in the buffer; one undo step), **Stage
Hunk** (staging-area mode: only that change goes into the index, the file
on disk is untouched), **Show Diff** (the file's diff tab, scrolled to the
change; it shows the saved file) and **✕** (or `Escape`). Keys: Next /
Previous Change `Ctrl+Alt+Shift+Down` / `Up` (PyCharm; VS Code's
`Alt+F5` / `Shift+Alt+F5` too; Go menu), **Rollback Lines** `Ctrl+Alt+Z`
(every change touching the selected lines; Git menu).

**Annotate with Git Blame** (right-click in the gutter, Git menu or the
palette; again to close) adds a column left of the line numbers with the
date and author of the commit that last changed each line, written once
per run of lines from the same commit (commits of the last 30 days in the
normal text color, older ones muted). Hovering shows the commit's hash,
author, date and full message; clicking selects it in the Git tool window
(filters cleared). Blame runs on the editor's text, so unsaved lines read
"Not committed", and lines move with your edits; it runs again after a
commit. With `"git": { "inline_blame": true }` the cursor's line also shows
`author, age • summary` after its end (VS Code's inline blame).
The gutter's right-click menu also has Rollback Lines and Show Change on a
changed line.
