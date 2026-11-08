# Azure DevOps

Azure DevOps is an optional built-in extension for `dev.azure.com` (Azure
DevOps Services). Enable **Azure DevOps** in **File > Preferences > Manage
Extensions…**, then choose **File > Open Azure DevOps** (or run it from the
command palette).

Each Azure DevOps project opens in a window tab of its own, beside the
project tabs, named after it (`contoso / Web`). A tab opens loaded and is
restored with the window. Opening it again shows the tab that is already
open.

## Connect with Microsoft SSO

Install [Azure CLI](https://learn.microsoft.com/en-us/cli/azure/install-azure-cli)
on the computer running the editor and make `az` available on its PATH.
The Azure DevOps CLI extension is not required.

**Open Azure DevOps** finds the Azure DevOps project from the shown
project's `origin` Git remote, and its current branch for running
pipelines. HTTPS, Azure SSH and legacy `organization.visualstudio.com`
remotes are supported. Without such a remote it opens the Azure DevOps
project loaded last, or an empty tab. Edit the organization (`contoso`, not
a URL), project, repository and tenant under **Connection** at the bottom of
the sidebar. Leave Repository empty to list open PRs across the project.
Tenant is an optional Microsoft Entra tenant ID or domain; set it before
signing in if your organization requires it. Loading another organization
or project moves the tab to that project.

Choose **Sign in with Microsoft** and complete Azure CLI's browser sign-in,
including any MFA required by your organization. Sign-in allows accounts
without an Azure subscription. An existing `az login` session can be reused
without signing in again. **Load** (then **Refresh**) applies the connection
and fetches PRs and pipelines.

Authentication uses Microsoft Entra tokens for the Azure DevOps resource.
Azure CLI manages the shared account session, token cache and renewal.
NextCode does not request a PAT, store tokens, or require its own Entra
application registration. Sign-in changes the shared local Azure CLI
session; use `az logout` outside NextCode to sign out. See Microsoft's
[Entra token guidance](https://learn.microsoft.com/en-us/azure/devops/cli/entra-tokens).

Each Azure DevOps project's organization, project, repository and tenant
are saved in `<config>/next_code/azure-devops/<organization>/<project>.json`
(the file its tab shows); branches and run parameters are not saved.

For SSH, WSL and container projects, Git discovery runs where the project
lives. Azure CLI and authentication always run on the local desktop; no
Azure tokens are forwarded to the remote project.

## The tab

The sidebar lists the project's **Pull requests** and its **Pipelines**,
with the connection below them. They scroll on their own, so the sidebar
stays in place. The page beside it has the selected item's heading, with
**Refresh** (the pull requests and pipelines, or a pipeline's runs) and
**Sign in with Microsoft**, plus **Cancel** while a request runs. Errors
and notices appear below the heading. In a focused table, Up and Down move
between rows and Enter opens one, like a click.

## Pull requests

Tiles sum up the open PRs: how many there are (and how many are drafts),
how many are ready to merge, how many have failing checks or unresolved
comments, and the oldest. The table lists every active PR with its author,
source and target branches, repository (when listing a whole project) and
age, plus:

- **Approvals**: an icon a reviewer: a green tick for each approval, a
  yellow clock for "waiting for the author", a red cross for a reject, and an
  empty ring for each approval the branch's minimum-reviewers policy still
  needs. `✓ ✓ ○` is two approvals of three. Hover an icon for who it is.
- **Checks**: the PR's other branch policies, such as build validation and
  status checks: a red cross and the failing check's name (or how many
  fail), else a clock and how many are pending, else a tick and how many
  passed. Hover the icon for their names.
- **Comments**: comment threads people started, and how many are still
  open (`2 open of 5`, the icon yellow while any are).

The dot at the start of a row sums it up: red for a failing check or a
reject, yellow for unresolved comments or "waiting for author", blue while
checks run, and green when the PR is ready to merge. Drafts have none.

Votes come with the list. Checks and comments take two more requests a PR,
read eight at a time after the list appears (for the first 100 PRs); the
cells show `…` until theirs arrive, and the rest of the tab stays usable
meanwhile. Click a PR to open its review page in the system browser.
Refresh explicitly to update the list. Native review diffs and commenting
are not part of it.

## Pipelines

The sidebar lists up to 100 build definitions; click one to fetch its latest
30 runs. Tiles show the last run's outcome, the success rate and median
duration of completed runs, and how many runs are in progress. **Duration of
completed runs** plots each run by when it was queued, colored by outcome;
click a point to open that run.

To queue a YAML pipeline, open **Run this pipeline**, enter the branch and
optional **Parameters** JSON, and choose **Run**. Parameters must be a JSON
object, for example `{"environment":"staging","runTests":true}`. Running
uses Azure's Pipelines Runs API; classic release pipelines are not
supported.

Click a run to view its stage/job/task hierarchy, with each record's status,
progress, duration and error/warning counts. Active runs update every ten
seconds; completed runs stop polling. **Refresh** beside the run updates it
manually, and **Open in Azure DevOps** opens it in the browser. Click a
record marked **View log** to load its log into a selectable, copyable pane
at the bottom of the tab, showing the last 2,000 lines of large logs. The pane's
**Refresh** loads the log again; **Close** hides it.

The signed-in user needs the usual Azure DevOps project/repository and
pipeline permissions. Some organizations restrict Azure CLI sign-in through
Conditional Access; failures appear in the tab. A queued build can take a
few seconds to expose a timeline. Refresh while it initializes.

Network calls have a 90-second timeout; sign-in allows five minutes.
**Cancel** ends the local request, not an already queued pipeline run. A
queue request that times out may still have succeeded: refresh the run
history before queuing again. NextCode does not automatically retry queue
requests. Disabling the extension or closing its tab cancels local CLI work and
stops polling.
