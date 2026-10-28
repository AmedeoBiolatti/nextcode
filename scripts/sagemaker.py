#!/usr/bin/env python3
"""SageMaker SSH-over-SSM proxy for Nextcode (Python standard library only).

`config ALIAS ...` prints an OpenSSH stanza; `proxy ...` carries SSH bytes.
Never print AWS responses: they contain credentials and session tokens.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys


SPACE_ARN = re.compile(
    r"arn:(aws(?:-[a-z]+)*):sagemaker:([a-z0-9-]+):[0-9]{12}:space/([A-Za-z0-9_-]+)/([A-Za-z0-9_-]+)\Z"
)


def aws_json(arguments, region, profile=None, credentials=None):
    program = shutil.which("aws")
    if not program:
        raise ValueError("Install AWS CLI v2 and put aws on PATH")
    command = [program, *arguments, "--region", region, "--output", "json", "--no-cli-pager"]
    environment = os.environ.copy()
    environment["AWS_CLI_AUTO_PROMPT"] = "off"
    if credentials is not None:
        # Explicit project credentials must take precedence over any local profile.
        environment.pop("AWS_PROFILE", None)
        environment.pop("AWS_DEFAULT_PROFILE", None)
        environment.update(credentials)
    elif profile:
        command += ["--profile", profile]
    result = subprocess.run(command, env=environment, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    if result.returncode:
        # Do not echo service responses or credential_process output into logs.
        code = re.search(r"An error occurred \(([A-Za-z0-9]+)\)",
                         result.stderr.decode("utf-8", errors="replace"))
        detail = f" ({code[1]})" if code else ""
        raise ValueError(f"AWS {' '.join(arguments[:2])} failed{detail}. Check your login, permissions, "
                         "region and that the Space is running with Remote Access enabled")
    try:
        response = json.loads(result.stdout)
    except (ValueError, UnicodeError):
        raise ValueError("AWS CLI returned an invalid JSON response") from None
    if not isinstance(response, dict):
        raise ValueError("AWS CLI returned an invalid JSON response")
    return response


def credentials_env(credentials):
    if not isinstance(credentials, dict):
        raise ValueError("Invalid Unified Studio project credentials; sign in again")
    fields = {
        "AWS_ACCESS_KEY_ID": credentials.get("accessKeyId"),
        "AWS_SECRET_ACCESS_KEY": credentials.get("secretAccessKey"),
        "AWS_SESSION_TOKEN": credentials.get("sessionToken"),
    }
    if not all(isinstance(value, str) and value for value in fields.values()):
        raise ValueError("Missing Unified Studio project credentials; sign in again")
    return fields


def toolkit_credentials(arn):
    """Explicit opt-in to the Toolkit's project credential cache, not its server.

    AWS Toolkit refreshes this cache while signed in. Reading it avoids relying
    on a VS Code process, its environment injection or a detached HTTP server.
    """
    path = Path.home() / ".aws" / ".sagemaker-space-profiles"
    try:
        mapping = json.loads(path.read_text(encoding="utf-8"))
        profile = mapping["localCredential"][arn]
        credentials = mapping["smusProjects"][profile["smusProjectId"]]
        return credentials_env({"accessKeyId": credentials["accessKey"],
                                "secretAccessKey": credentials["secret"],
                                "sessionToken": credentials["token"]})
    except (OSError, ValueError, KeyError, TypeError):
        raise ValueError("No Unified Studio project credentials in the AWS Toolkit cache. "
                         "Sign in and connect to this Space with AWS Toolkit first") from None


def project_credentials(options, region):
    response = aws_json([
        "datazone", "list-connections", "--domain-identifier", options.domain_id,
        "--project-identifier", options.project_id, "--type", "IAM",
    ], region, options.profile)
    connections = response.get("items", [])
    if not isinstance(connections, list) or not all(isinstance(item, dict) for item in connections):
        raise ValueError("DataZone returned an invalid connection list")
    connection = next((item for name in ("project.iam", "default.iam")
                       for item in connections if item.get("name") == name), None)
    if not connection or not connection.get("connectionId"):
        raise ValueError("No project.iam or default.iam connection found for the Unified Studio project")
    response = aws_json([
        "datazone", "get-connection", "--domain-identifier", options.domain_id,
        "--identifier", connection["connectionId"], "--with-secret",
    ], region, options.profile)
    return credentials_env(response.get("connectionCredentials", {}))


def proxy(options):
    region = SPACE_ARN.fullmatch(options.space_arn)[2]
    plugin = shutil.which("session-manager-plugin")
    if not plugin:
        raise ValueError("Install the AWS Session Manager plugin and put session-manager-plugin on PATH")
    credentials = (toolkit_credentials(options.space_arn) if options.toolkit else
                   project_credentials(options, region) if options.domain_id else None)
    session = aws_json(["sagemaker", "start-session", "--resource-identifier", options.space_arn],
                       region, options.profile, credentials)
    if not all(isinstance(session.get(key), str) and session[key]
               for key in ("SessionId", "StreamUrl", "TokenValue")):
        raise ValueError("SageMaker returned an incomplete session response")
    payload = json.dumps({"sessionId": session["SessionId"], "streamUrl": session["StreamUrl"],
                          "tokenValue": session["TokenValue"]}, separators=(",", ":"))
    # Inherit stdio: SSH requires a byte stream; no status output belongs here.
    return subprocess.call([plugin, payload, region, "StartSession"])


def ssh_config(options):
    command = [sys.executable, str(Path(__file__).resolve()), "proxy", "--space-arn", options.space_arn]
    for name in ("profile", "domain_id", "project_id"):
        value = getattr(options, name)
        if value:
            command += ["--" + name.replace("_", "-"), value]
    if options.toolkit:
        command.append("--toolkit")
    quoted = subprocess.list2cmdline(command) if os.name == "nt" else shlex.join(command)
    # OpenSSH expands percent tokens even inside shell quotes.
    quoted = quoted.replace("%", "%%")
    if any(c in quoted for c in "\r\n\0"):
        raise ValueError("SSH configuration cannot contain newlines or NULs")
    return (f"Host {options.alias}\n"
            f"    HostName {options.alias}\n"
            "    User sagemaker-user\n"
            f"    ProxyCommand {quoted}\n"
            "    ForwardAgent no\n"
            "    StrictHostKeyChecking accept-new\n"
            "    ConnectTimeout 60\n")


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    for name in ("config", "proxy"):
        command = commands.add_parser(name)
        if name == "config":
            command.add_argument("alias", help="SSH alias to show in Nextcode's Remote SSH picker")
        command.add_argument("--space-arn", required=True)
        command.add_argument("--profile", help="AWS CLI profile (classic Studio or DataZone caller)")
        command.add_argument("--domain-id", help="Unified Studio DataZone domain ID (dzd_…)")
        command.add_argument("--project-id", help="Unified Studio project ID")
        command.add_argument("--toolkit", action="store_true",
                             help="Use Unified Studio project credentials from an existing AWS Toolkit sign-in")
    return result


def main(argv=None):
    arguments = parser()
    options = arguments.parse_args(argv)
    if not SPACE_ARN.fullmatch(options.space_arn):
        arguments.error("--space-arn must be a SageMaker Space ARN (space/domain-id/space-name)")
    if bool(options.domain_id) != bool(options.project_id):
        arguments.error("--domain-id and --project-id must be provided together")
    if options.toolkit and (options.domain_id or options.profile):
        arguments.error("--toolkit cannot be combined with --profile or --domain-id")
    if options.command == "config" and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", options.alias):
        arguments.error("alias must start with a letter or digit and contain only letters, digits, _, . or -")
    try:
        if options.command == "config":
            print(ssh_config(options), end="")
            return 0
        return proxy(options)
    except (OSError, ValueError, subprocess.TimeoutExpired):
        # Keep exceptions with service data out of tracebacks, including tokens.
        error = sys.exc_info()[1]
        message = str(error) if isinstance(error, ValueError) else "Could not run AWS CLI or Session Manager plugin"
        print(f"SageMaker: {message}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
