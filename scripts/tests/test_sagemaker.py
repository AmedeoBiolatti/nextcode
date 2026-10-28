import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("sagemaker", Path(__file__).parents[1] / "sagemaker.py")
sagemaker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sagemaker)
ARN = "arn:aws:sagemaker:eu-west-1:123456789012:space/d-example/my-space"
SESSION = {"SessionId": "session", "StreamUrl": "wss://example", "TokenValue": "secret-token"}
CREDS = {"accessKeyId": "project-key", "secretAccessKey": "project-secret", "sessionToken": "project-token"}


class SageMakerTests(unittest.TestCase):
    def options(self, *extra):
        return sagemaker.parser().parse_args(["proxy", "--space-arn", ARN, *extra])

    @patch.object(sagemaker.shutil, "which", return_value="plugin")
    @patch.object(sagemaker.subprocess, "call", return_value=0)
    @patch.object(sagemaker, "aws_json", return_value=SESSION)
    def test_classic_proxy_region_profile_and_plugin_stream(self, aws, plugin, _):
        self.assertEqual(sagemaker.proxy(self.options("--profile", "training")), 0)
        self.assertEqual(aws.call_args.args,
                         (["sagemaker", "start-session", "--resource-identifier", ARN],
                          "eu-west-1", "training", None))
        args = plugin.call_args.args[0]
        self.assertEqual(args[0], "plugin")
        self.assertEqual(json.loads(args[1]), {"sessionId": "session", "streamUrl": "wss://example", "tokenValue": "secret-token"})
        self.assertEqual(args[2:], ["eu-west-1", "StartSession"])
        self.assertEqual(plugin.call_args.kwargs, {})  # inherit SSH stdin/stdout

    @patch.object(sagemaker.shutil, "which", return_value="plugin")
    @patch.object(sagemaker.subprocess, "call", return_value=0)
    @patch.object(sagemaker, "aws_json")
    def test_unified_prefers_project_iam_and_exchanges_credentials(self, aws, plugin, _):
        aws.side_effect = [
            {"items": [{"name": "default.iam", "connectionId": "default"},
                       {"name": "project.iam", "connectionId": "project"}]},
            {"connectionCredentials": CREDS}, SESSION,
        ]
        self.assertEqual(sagemaker.proxy(self.options("--domain-id", "dzd_test", "--project-id", "project1", "--profile", "domain")), 0)
        self.assertIn("project", aws.call_args_list[1].args[0])
        self.assertEqual(aws.call_args_list[2].args[3], sagemaker.credentials_env(CREDS))

    @patch.object(sagemaker, "aws_json", return_value={"items": []})
    def test_unified_no_connection_does_not_start_session(self, aws):
        with self.assertRaisesRegex(ValueError, "No project.iam"):
            sagemaker.project_credentials(self.options("--domain-id", "dzd_test", "--project-id", "project1"), "eu-west-1")
        self.assertEqual(aws.call_count, 1)

    @patch.object(sagemaker.Path, "read_text")
    def test_toolkit_uses_matching_project_not_another_spaces_credentials(self, read):
        read.return_value = json.dumps({"localCredential": {ARN: {"smusProjectId": "p1"}},
                                      "smusProjects": {"p1": {"accessKey": "key", "secret": "secret", "token": "token"},
                                                       "p2": {"accessKey": "wrong"}}})
        self.assertEqual(sagemaker.toolkit_credentials(ARN)["AWS_ACCESS_KEY_ID"], "key")
        with self.assertRaisesRegex(ValueError, "AWS Toolkit"):
            sagemaker.toolkit_credentials(ARN + "-other")

    @patch.object(sagemaker.shutil, "which", return_value="aws")
    @patch.object(sagemaker.subprocess, "run")
    def test_project_credentials_override_ambient_profile(self, run, _):
        run.return_value = subprocess.CompletedProcess([], 0, b"{}", b"")
        with patch.dict(os.environ, {"AWS_PROFILE": "wrong", "AWS_DEFAULT_PROFILE": "wrong"}):
            sagemaker.aws_json(["sagemaker", "start-session"], "eu-west-1", "wrong", sagemaker.credentials_env(CREDS))
        self.assertNotIn("--profile", run.call_args.args[0])
        env = run.call_args.kwargs["env"]
        self.assertNotIn("AWS_PROFILE", env)
        self.assertNotIn("AWS_DEFAULT_PROFILE", env)
        self.assertEqual(env["AWS_ACCESS_KEY_ID"], "project-key")

    @patch.object(sagemaker.shutil, "which", return_value="aws")
    @patch.object(sagemaker.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, b"token-value", b"secret-value"))
    def test_aws_error_does_not_disclose_responses(self, *_):
        with self.assertRaises(ValueError) as error:
            sagemaker.aws_json(["sagemaker", "start-session"], "eu-west-1")
        self.assertNotIn("secret-value", str(error.exception))
        self.assertNotIn("token-value", str(error.exception))

    def test_config_is_discoverable_ssh_alias_and_contains_no_credentials(self):
        args = sagemaker.parser().parse_args(["config", "training", "--space-arn", ARN, "--toolkit"])
        config = sagemaker.ssh_config(args)
        self.assertIn("Host training\n", config)
        self.assertIn("User sagemaker-user\n", config)
        self.assertIn(" proxy --space-arn ", config)
        self.assertIn("--toolkit", config)
        self.assertNotIn("StrictHostKeyChecking no", config)

    @unittest.skipUnless(sagemaker.shutil.which("ssh"), "OpenSSH is not installed")
    def test_generated_config_is_readable_by_real_openssh(self):
        options = sagemaker.parser().parse_args(["config", "training", "--space-arn", ARN, "--profile", "a profile"])
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config"
            config.write_text(sagemaker.ssh_config(options), encoding="utf-8")
            result = subprocess.run(["ssh", "-G", "-F", str(config), "training"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("user sagemaker-user\n", result.stdout)
        self.assertIn("proxycommand ", result.stdout)
        self.assertIn("--space-arn " + ARN, result.stdout)

    def test_bad_arn_alias_and_incompatible_auth_options_are_rejected(self):
        cases = [
            ["config", "bad\nHost evil", "--space-arn", ARN],
            ["proxy", "--space-arn", "not-an-arn"],
            ["proxy", "--space-arn", ARN, "--domain-id", "dzd_test"],
            ["proxy", "--space-arn", ARN, "--toolkit", "--profile", "wrong"],
        ]
        for args in cases:
            with self.subTest(args=args), patch("sys.stderr", new_callable=io.StringIO), self.assertRaises(SystemExit):
                sagemaker.main(args)

    @patch.object(sagemaker.shutil, "which", return_value="plugin")
    @patch.object(sagemaker.subprocess, "call")
    @patch.object(sagemaker, "aws_json", return_value={"SessionId": "only-one-field"})
    def test_incomplete_session_never_launches_plugin(self, _, plugin, __):
        with self.assertRaisesRegex(ValueError, "incomplete session"):
            sagemaker.proxy(self.options())
        plugin.assert_not_called()


if __name__ == "__main__":
    unittest.main()
