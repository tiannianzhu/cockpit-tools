"""Offline contract tests for the remote Codex bundle generator."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch


SOURCE = Path(__file__).resolve().parents[1] / "src-tauri/src/modules/remote_api_bundle.py"
SPEC = importlib.util.spec_from_file_location("remote_api_bundle", SOURCE)
bundle = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bundle)


TEMPLATE = {
    "slug": "official-base", "display_name": "Official", "description": "Sample",
    "model_messages": {"instructions_template": "Keep this exact template."},
    "default_reasoning_level": "medium",
    "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}],
    "context_window": 12000, "input_modalities": ["text", "image"],
    "use_responses_lite": False, "supported_in_api": True,
    "visibility": "list", "shell_type": "shell_command",
}


def api(models=None):
    return {
        "provider_name": "Example Provider",
        "base_url": "https://provider.example/v1", "wire_api": "responses",
        "supports_websockets": False,
        "model_catalog_definition": {
            "base_model": TEMPLATE["slug"],
            "models": [{"context_window": 12000,
                        "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}],
                        "default_reasoning_level": "medium", **model}
                       for model in (models if models is not None else [{"slug": "remote-a"}])],
        },
    }


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.calls = []

        def run(command, **kwargs):
            self.calls.append((command, kwargs))
            if "--bundled" in command:
                value = {"models": self.bundled}
            else:
                value = self.effective
            return subprocess.CompletedProcess(command, 0, json.dumps(value), "")

        self.patches = [patch.object(bundle, "_cli", return_value="/mock/codex"),
                        patch.object(bundle.subprocess, "run", side_effect=run)]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)
        self.effective = {"models": []}
        self.bundled = [TEMPLATE]

    def prepare(self, definition=None):
        return bundle.prepare_api_bundle(self.home, definition or api())

    def test_merge_preserves_other_settings_and_selected_model(self):
        original = (
            'model = "remote-b"\nmodel_provider = "old"\n'
            'model_catalog_json = "old.json"\napproval_policy = "never"\n'
            '[features]\nweb_search = true\n'
            '[model_providers.other]\nbase_url = "https://other.example"\n'
            '[model_providers.cockpit_remote]\nbase_url = "https://old.example"\n'
            'wire_api = "responses"\n'
            '[profiles.safe]\napproval_policy = "on-request"\n'
        )
        (self.home / "config.toml").write_text(original)
        prepared = self.prepare(api([{"slug": "remote-a"}, {"slug": "remote-b"}]))
        self.assertEqual((self.home / "config.toml").read_text(), original)
        config = tomllib.loads(prepared["config.toml"].decode())
        self.assertEqual(config["model"], "remote-b")
        self.assertEqual(config["approval_policy"], "never")
        self.assertEqual(config["features"], {"web_search": True})
        self.assertEqual(config["model_providers"]["other"]["base_url"], "https://other.example")
        self.assertEqual(config["profiles"]["safe"]["approval_policy"], "on-request")
        self.assertEqual(config["model_providers"]["cockpit_remote"], {
            "name": "Example Provider",
            "base_url": "https://provider.example/v1", "wire_api": "responses",
            "requires_openai_auth": True, "supports_websockets": False,
        })

    def test_equivalent_old_provider_is_removed_without_touching_plugins(self):
        original = (
            'model_provider = "prior"\n'
            '[model_providers.prior]\nname = "Old Name"\n'
            'base_url = "https://provider.example/v1"\nwire_api = "responses"\n'
            'requires_openai_auth = true\nsupports_websockets = false\n'
            '[marketplaces.example]\nsource_type = "local"\nsource = "/example"\n'
            '[plugins."example@local"]\nenabled = true\n'
        )
        (self.home / "config.toml").write_text(original)
        config = tomllib.loads(self.prepare()["config.toml"].decode())
        self.assertEqual(set(config["model_providers"]), {bundle.PROVIDER})
        self.assertEqual(config["model_providers"][bundle.PROVIDER]["name"], api()["provider_name"])
        before = tomllib.loads(original)
        for key in ("marketplaces", "plugins"):
            self.assertEqual(config[key], before[key])
        self.assertEqual((self.home / "config.toml").read_text(), original)

    def test_same_endpoint_with_distinct_options_is_not_removed(self):
        original = (
            '[model_providers.prior]\nname = "Prior"\n'
            'base_url = "https://provider.example/v1"\nwire_api = "responses"\n'
            'requires_openai_auth = true\nsupports_websockets = false\n'
            'request_max_retries = 8\n'
        )
        (self.home / "config.toml").write_text(original)
        config = tomllib.loads(self.prepare()["config.toml"].decode())
        self.assertEqual(config["model_providers"]["prior"], tomllib.loads(original)["model_providers"]["prior"])
        (self.home / "config.toml").write_text(original + '[profiles.saved]\nmodel_provider = "prior"\n')
        with self.assertRaisesRegex(ValueError, "profile"):
            self.prepare()

    def test_generated_reasoning_order_is_canonical_and_default_preserved(self):
        levels = [{"effort": effort, "description": effort} for effort in ["max", "xhigh", "high", "none", "low", "minimal"]]
        prepared = self.prepare(api([{"slug": "sample", "supported_reasoning_levels": levels, "default_reasoning_level": "max"}]))
        model = json.loads(prepared[bundle.CATALOG_NAME])["models"][0]
        self.assertEqual([level["effort"] for level in model["supported_reasoning_levels"]], ["none", "minimal", "low", "high", "xhigh", "max"])
        self.assertEqual(model["default_reasoning_level"], "max")
        self.assertEqual(levels[0]["effort"], "max")

    def test_model_id_and_display_name_are_independent(self):
        prepared = self.prepare(api([
            {"slug": "upstream-id", "display_name": "Readable model"},
            {"slug": "no-label"}, {"slug": "blank-label", "display_name": " "},
        ]))
        models = json.loads(prepared[bundle.CATALOG_NAME])["models"]
        self.assertEqual([(m["slug"], m["display_name"]) for m in models], [
            ("upstream-id", "Readable model"), ("no-label", "no-label"), ("blank-label", "blank-label"),
        ])
        self.assertEqual(models[1]["description"], "no-label")

    def test_provider_display_name_is_preserved_without_changing_internal_id(self):
        definition = api()
        definition["provider_name"] = '示例 "Provider"'
        prepared = self.prepare(definition)
        config = tomllib.loads(prepared["config.toml"].decode())
        self.assertEqual(config["model_provider"], bundle.PROVIDER)
        self.assertEqual(config["model_providers"][bundle.PROVIDER]["name"], definition["provider_name"])
        definition["provider_name"] = " "
        with self.assertRaisesRegex(ValueError, "provider_name"):
            self.prepare(definition)

    def test_models_clone_template_with_image_reasoning_and_null_removal(self):
        prepared = self.prepare(api([
            {"slug": "text-only", "input_modalities": ["text"],
             "display_name": "Text", "description": None},
            {"slug": "image-enabled", "default_reasoning_level": "low",
             "use_responses_lite": True},
        ]))
        models = json.loads(prepared[bundle.CATALOG_NAME])["models"]
        self.assertEqual(models[0]["model_messages"], TEMPLATE["model_messages"])
        self.assertNotIn("description", models[0])
        self.assertEqual(models[0]["input_modalities"], ["text"])
        self.assertFalse(models[0]["use_responses_lite"])
        self.assertEqual(models[1]["input_modalities"], ["text", "image"])
        self.assertEqual(models[1]["default_reasoning_level"], "low")
        self.assertTrue(models[1]["use_responses_lite"])
        self.assertEqual(self.calls[0][0][-1], "--bundled")
        self.assertEqual(self.calls[0][1]["env"]["CODEX_HOME"], str(self.home.resolve()))

    def test_generated_priorities_follow_list_order_even_after_reordering(self):
        self.bundled = [{**TEMPLATE, "priority": 99}]
        overrides = [{"slug": "first", "priority": 7},
                     {"slug": "second", "priority": 0},
                     {"slug": "third"}]
        for ordered in (overrides, list(reversed(overrides))):
            prepared = self.prepare(api(ordered))
            models = json.loads(prepared[bundle.CATALOG_NAME])["models"]
            self.assertEqual([m["slug"] for m in models], [m["slug"] for m in ordered])
            self.assertEqual([m["priority"] for m in models], [0, 1, 2])
            self.assertEqual(sorted(models, key=lambda m: m["priority"]), models)
        self.assertEqual(overrides[0]["priority"], 7)

    def test_auto_selects_best_visible_official_template_independent_of_transport(self):
        def official(slug, priority, **fields):
            model = {**TEMPLATE, "slug": slug, "priority": priority,
                     "supported_in_api": True, "visibility": "list",
                     "shell_type": "shell_command", "use_responses_lite": False,
                     "model_messages": {"instructions_template": "Template " + slug}}
            model.update(fields)
            return model

        self.bundled = [
            official("z-tie", 2),
            official("hidden", 0, visibility="hidden"),
            official("unsupported", 0, supported_in_api=False),
            official("no-instructions", 0, model_messages={}),
            official("chosen", 2, use_responses_lite=True),
            official("lower-priority", 3),
        ]
        definition = api([{"slug": "remote-a", "context_window": 64000,
                           "supported_reasoning_levels": [{"effort": "low"}],
                           "default_reasoning_level": "low",
                           "input_modalities": ["text"]}])
        definition["model_catalog_definition"]["base_model"] = "auto"
        prepared = self.prepare(definition)
        model = json.loads(prepared[bundle.CATALOG_NAME])["models"][0]
        self.assertEqual(model["model_messages"]["instructions_template"], "Template chosen")
        self.assertEqual(model["context_window"], 64000)
        self.assertEqual(model["input_modalities"], ["text"])
        self.assertFalse(model["use_responses_lite"])

    def test_auto_rejects_incomplete_explicit_model_parameters(self):
        self.bundled = [{**TEMPLATE, "supported_in_api": True,
                         "visibility": "list", "shell_type": "shell_command",
                         "use_responses_lite": False}]
        definition = api([{"slug": "remote-a", "input_modalities": ["text"]}])
        definition["model_catalog_definition"]["base_model"] = "auto"
        definition["model_catalog_definition"]["models"][0].pop("context_window", None)
        with self.assertRaisesRegex(ValueError, "positive context_window"):
            self.prepare(definition)

        definition["model_catalog_definition"]["models"][0]["context_window"] = 64000
        definition["model_catalog_definition"]["models"][0].pop("supported_reasoning_levels", None)
        with self.assertRaisesRegex(ValueError, "supported_reasoning_levels"):
            self.prepare(definition)

    def test_auto_fails_when_no_suitable_official_template_exists(self):
        self.bundled = [{**TEMPLATE, "supported_in_api": True,
                         "visibility": "hidden", "shell_type": "shell_command",
                         "use_responses_lite": True}]
        definition = api()
        definition["model_catalog_definition"]["base_model"] = "auto"
        with self.assertRaisesRegex(ValueError, "no suitable official bundled template"):
            self.prepare(definition)

    def test_third_party_models_drop_official_upgrade_metadata(self):
        self.bundled = [{**TEMPLATE, "upgrade": {"model": "official-next"}}]
        prepared = self.prepare(api([
            {"slug": "inherited"},
            {"slug": "imported", "upgrade": {"model": "old-official-target"}},
        ]))
        models = json.loads(prepared[bundle.CATALOG_NAME])["models"]
        for model in models:
            self.assertIn("upgrade", model)
            self.assertIsNone(model["upgrade"])
            self.assertEqual(model["model_messages"], TEMPLATE["model_messages"])
            self.assertEqual(model["input_modalities"], TEMPLATE["input_modalities"])

    def test_editor_definition_requires_explicit_parameters_with_named_template(self):
        definition = api()
        definition["model_catalog_definition"]["require_explicit_capabilities"] = True
        definition["model_catalog_definition"]["models"][0].pop("context_window", None)
        with self.assertRaisesRegex(ValueError, "positive context_window"):
            self.prepare(definition)

    def test_invalid_definitions(self):
        cases = [
            [{"slug": "same"}, {"slug": "SAME"}],
            [{"slug": "a", "context_window": 0}],
            [{"slug": "a", "context_window": True}],
            [{"slug": "a", "default_reasoning_level": "none"}],
            [{"slug": "a", "input_modalities": ["image"]}],
            [{"slug": "a", "input_modalities": ["text", "audio"]}],
        ]
        for models in cases:
            with self.subTest(models=models), self.assertRaises(ValueError):
                self.prepare(api(models))
        bad = api()
        bad["model_catalog_definition"]["base_model"] = "missing"
        self.prepare(bad)  # Legacy template names are ignored, even when no longer installed.
        bad = api()
        bad["wire_api"] = "chat_completions"
        with self.assertRaisesRegex(ValueError, "responses"):
            self.prepare(bad)

    def test_auth_and_profile_conflicts_are_rejected(self):
        for content in (
            '[model_providers.cockpit_remote]\nenv_key = "SECRET"\n',
            '[profiles.other]\nmodel_provider = "other"\n',
            'forced_login_method = "chatgpt"\n',
            'cli_auth_credentials_store = "keyring"\n',
            'profile = "other"\n[profiles.other]\napproval_policy = "never"\n',
            'model_context_window = 10000\n',
            'model_auto_compact_token_limit = 9000\n',
        ):
            (self.home / "config.toml").write_text(content)
            with self.assertRaisesRegex(ValueError, "manual review"):
                self.prepare()

    def test_reasoning_effort_uses_selected_model_only(self):
        definition = api([
            {"slug": "first"},
            {"slug": "second", "default_reasoning_level": "low",
             "supported_reasoning_levels": [{"effort": "low"}]},
        ])
        for selected, effort, expected in [
            ("first", "medium", "medium"),  # Other models need not support it.
            ("second", "medium", "low"),  # Selected model requires fallback.
            ("removed", "low", "medium"),  # New model uses its own default.
        ]:
            with self.subTest(selected=selected, effort=effort):
                (self.home / "config.toml").write_text(
                    f'model = "{selected}"\nmodel_reasoning_effort = "{effort}"\nsandbox_mode = "workspace-write"\n')
                prepared = self.prepare(definition)
                config = tomllib.loads(prepared["config.toml"].decode())
                self.assertEqual(config["model_reasoning_effort"], expected)
                self.assertEqual(config["model"], "first" if selected == "removed" else selected)
                self.assertEqual(config["sandbox_mode"], "workspace-write")

    def test_missing_global_effort_stays_unset_when_model_changes(self):
        for model in ["first", "removed"]:
            (self.home / "config.toml").write_text(f'model = "{model}"\n')
            config = tomllib.loads(self.prepare(api([{"slug": "first"}]))["config.toml"].decode())
            self.assertNotIn("model_reasoning_effort", config)

    def test_repeated_sync_keeps_one_blank_line_before_managed_provider(self):
        path = self.home / "config.toml"
        path.write_text('[plugins."example@bundled"]\nenabled = true\n\n  \n\n')
        first = self.prepare()["config.toml"]
        self.assertIn(b'enabled = true\n\n[model_providers.cockpit_remote]', first)
        for _ in range(3):
            path.write_bytes(first)
            self.assertEqual(self.prepare()["config.toml"], first)

    def test_multiline_string_cannot_be_silently_modified(self):
        (self.home / "config.toml").write_text('notes = """\nmodel = "inside string"\n"""\n')
        with self.assertRaisesRegex(ValueError, "unrelated settings"):
            self.prepare()

    def test_validation_compares_full_effective_entry(self):
        prepared = self.prepare()
        for name, content in prepared.items():
            (self.home / name).write_bytes(content)
        expected = json.loads(prepared[bundle.CATALOG_NAME])
        self.effective = expected
        bundle.validate_api_bundle(self.home, api(), prepared)
        self.assertEqual(self.calls[-1][0][-2:], ["debug", "models"])
        self.effective = json.loads(json.dumps(expected))
        self.effective["models"][0]["model_messages"]["instructions_template"] = "changed"
        with self.assertRaisesRegex(ValueError, "differs.*fields model_messages"):
            bundle.validate_api_bundle(self.home, api(), prepared)
        (self.home / bundle.CATALOG_NAME).write_text("tampered")
        with self.assertRaisesRegex(ValueError, "differs"):
            bundle.validate_api_bundle(self.home, api(), prepared)


if __name__ == "__main__":
    unittest.main()
