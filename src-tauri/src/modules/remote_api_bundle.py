"""Prepare and verify a Codex third-party API config bundle without writing it.

The caller owns locking, atomic installation, rollback, and process management.
Python 3.11 or newer is required for the standard-library TOML parser.
"""

from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10 can still load the surrounding remote script.
    tomllib = None
from urllib.parse import urlsplit


CATALOG_NAME = "cockpit-model-catalog.json"
PROVIDER = "cockpit_remote"
_KEY = re.compile(r"^\s*([A-Za-z0-9_-]+)\s*=")
_HEADER = re.compile(r"^\s*\[([^\[\]]+)\]\s*(?:#.*)?$")
_ANY_HEADER = re.compile(r"^\s*\[\[?.*\]\]?\s*(?:#.*)?$")
_OWNED_KEYS = {"model", "model_provider", "model_catalog_json"}
_CONTEXT_OVERRIDES = {"model_context_window", "model_auto_compact_token_limit"}


def _require_python() -> None:
    if sys.version_info < (3, 11):
        raise RuntimeError("remote_api_bundle requires Python 3.11 or newer")


def _cli() -> str:
    local = Path.home() / ".local/bin/codex"
    if local.is_file() and os.access(local, os.X_OK):
        return str(local)
    found = shutil.which("codex")
    if not found:
        raise RuntimeError("Codex CLI is unavailable")
    return found


def _catalog(home: Path, *, bundled: bool) -> dict:
    command = [_cli(), "debug", "models"]
    if bundled:
        command.append("--bundled")
    env = os.environ.copy()
    env["CODEX_HOME"] = str(home)
    try:
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20, check=False)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError("Codex model catalog command failed or timed out") from exc
    if result.returncode:
        # stderr may contain an auth token or private provider address.
        raise RuntimeError("Codex model catalog command failed")
    try:
        catalog = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError("Codex model catalog output is not JSON") from exc
    if not isinstance(catalog, dict) or not isinstance(catalog.get("models"), list):
        raise ValueError("Codex model catalog has no models array")
    return catalog


def _definition(api: dict) -> list[dict]:
    if not isinstance(api, dict):
        raise ValueError("API definition must be an object")
    base_url = api.get("base_url")
    if not isinstance(base_url, str) or not base_url or any(c.isspace() for c in base_url):
        raise ValueError("base_url must be an HTTP(S) URL")
    url = urlsplit(base_url)
    if url.scheme not in {"http", "https"} or not url.netloc or url.username or url.password or url.fragment:
        raise ValueError("base_url must be an HTTP(S) URL without credentials or fragment")
    if api.get("wire_api") != "responses":
        raise ValueError("wire_api must be responses")
    if type(api.get("supports_websockets")) is not bool:
        raise ValueError("supports_websockets must be boolean")
    name = api.get("provider_name", "Cockpit Remote")
    if not isinstance(name, str) or not name.strip():
        raise ValueError("provider_name must be a nonempty string")
    definition = api.get("model_catalog_definition")
    if not isinstance(definition, dict):
        raise ValueError("model_catalog_definition must be an object")
    overrides = definition.get("models")
    if not isinstance(overrides, list) or not overrides:
        raise ValueError("models must be a nonempty array")
    return overrides


def _automatic_template(models: list) -> dict:
    """Choose the installed CLI's highest-priority usable official template."""
    candidates = []
    for model in models:
        if not isinstance(model, dict):
            continue
        messages = model.get("model_messages")
        instructions = messages.get("instructions_template") if isinstance(messages, dict) else None
        priority = model.get("priority")
        if (
            model.get("supported_in_api") is not True
            or model.get("visibility") != "list"
            or not isinstance(model.get("shell_type"), str)
            or not model["shell_type"]
            or not isinstance(instructions, str)
            or not instructions.strip()
            or not isinstance(model.get("slug"), str)
            or not model["slug"]
        ):
            continue
        # Lower official priority wins; slug makes ties stable across catalog order.
        sort_priority = priority if type(priority) in (int, float) else float("inf")
        candidates.append((sort_priority, model["slug"], model))
    if not candidates:
        raise ValueError("no suitable official bundled template is available")
    return min(candidates, key=lambda candidate: (candidate[0], candidate[1]))[2]


def _validate_auto_overrides(overrides: list[dict]) -> None:
    """Require user-specified capabilities that cannot be inferred for auto models."""
    for override in overrides:
        if not isinstance(override, dict):
            raise ValueError("each model override must be an object")
        context = override.get("context_window")
        levels = override.get("supported_reasoning_levels")
        default = override.get("default_reasoning_level")
        if type(context) is not int or context <= 0:
            raise ValueError(
                "auto models require a positive context_window; configure model parameters in the provider"
            )
        if (
            not isinstance(levels, list) or not levels
            or any(not isinstance(level, dict) or not isinstance(level.get("effort"), str)
                   or not level["effort"] for level in levels)
            or not isinstance(default, str)
            or default not in [level["effort"] for level in levels if isinstance(level, dict)]
        ):
            raise ValueError(
                "auto models require supported_reasoning_levels and a matching default_reasoning_level; "
                "configure model parameters in the provider"
            )


REASONING_EFFORT_ORDER = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]


def _models(template: dict, overrides: list[dict]) -> list[dict]:
    result: list[dict] = []
    seen: set[str] = set()
    for override in overrides:
        if not isinstance(override, dict):
            raise ValueError("each model override must be an object")
        slug = override.get("slug")
        if not isinstance(slug, str) or not slug.strip() or slug != slug.strip():
            raise ValueError("each model needs a nonempty slug")
        if slug.casefold() in seen:
            raise ValueError("model slugs must be unique")
        seen.add(slug.casefold())
        model = copy.deepcopy(template)
        for key, value in override.items():
            if not isinstance(key, str) or not key:
                raise ValueError("model override keys must be nonempty strings")
            if value is None:
                model.pop(key, None)
            else:
                model[key] = copy.deepcopy(value)
        # Official migration targets do not apply to third-party model IDs,
        # including when an imported definition contains stale upgrade metadata.
        # Codex serializes a missing upgrade as null in its effective catalog.
        # Emit the same representation so full-entry validation stays strict.
        model["upgrade"] = None
        # The provider's list order is authoritative, including after an upstream
        # refresh. Never inherit ordering from the template or imported metadata.
        model["priority"] = len(result)
        display_name = override.get("display_name")
        if display_name is not None and not isinstance(display_name, str):
            raise ValueError("display_name must be a string")
        model["display_name"] = display_name.strip() if display_name and display_name.strip() else slug
        if "description" not in override:
            model["description"] = model["display_name"]
        if model.get("slug") != slug:
            raise ValueError("model slug cannot be removed")
        for key in ("context_window", "max_context_window"):
            value = model.get(key)
            if value is not None and (type(value) is not int or value <= 0):
                raise ValueError(f"{key} must be a positive integer")
        modalities = model.get("input_modalities")
        if (not isinstance(modalities, list) or not modalities
                or any(x not in ("text", "image") for x in modalities)
                or len(set(modalities)) != len(modalities) or "text" not in modalities):
            raise ValueError("input_modalities must contain text and optional image")
        levels = model.get("supported_reasoning_levels")
        if not isinstance(levels, list) or not levels or any(
            not isinstance(level, dict) or not isinstance(level.get("effort"), str)
            for level in levels
        ):
            raise ValueError("supported_reasoning_levels must contain effort entries")
        efforts = [level["effort"] for level in levels]
        if len(set(efforts)) != len(efforts) or model.get("default_reasoning_level") not in efforts:
            raise ValueError("default_reasoning_level must be supported")
        if any(effort not in REASONING_EFFORT_ORDER for effort in efforts):
            raise ValueError("unsupported Codex reasoning effort")
        levels.sort(key=lambda level: REASONING_EFFORT_ORDER.index(level["effort"]))
        if type(model.get("use_responses_lite")) is not bool:
            raise ValueError("use_responses_lite must be boolean")
        result.append(model)
    return result


def _config(original: str, api: dict, models: list[dict], home: Path) -> str:
    try:
        parsed = tomllib.loads(original)
    except tomllib.TOMLDecodeError as exc:
        raise ValueError("existing config.toml is invalid") from exc
    providers = parsed.get("model_providers", {})
    if not isinstance(providers, dict):
        raise ValueError("model_providers must be a table")
    existing = providers.get(PROVIDER, {})
    if not isinstance(existing, dict):
        raise ValueError("existing cockpit_remote provider must be a table")
    if any(key in existing for key in ("env_key", "http_headers", "env_http_headers", "experimental_bearer_token")):
        raise ValueError("existing cockpit_remote auth settings require manual review")
    if any(key in parsed for key in _CONTEXT_OVERRIDES):
        raise ValueError("global model context overrides require manual review")
    if parsed.get("forced_login_method") not in (None, "api"):
        raise ValueError("forced login method requires manual review")
    if parsed.get("cli_auth_credentials_store") not in (None, "file"):
        raise ValueError("credential store requires manual review")
    if any(key in parsed for key in ("forced_chatgpt_workspace_id", "openai_base_url")):
        raise ValueError("global auth settings require manual review")
    if "profile" in parsed:
        raise ValueError("active profile requires manual review")
    profiles = parsed.get("profiles", {})
    if isinstance(profiles, dict) and any(
        isinstance(profile, dict) and any(k in profile for k in _OWNED_KEYS | _CONTEXT_OVERRIDES | {
            "model_reasoning_effort",
            "forced_login_method", "cli_auth_credentials_store", "openai_base_url"
        })
        for profile in profiles.values()
    ):
        raise ValueError("profile model settings require manual review")
    # Remove only equivalent, standalone provider entries. Extra transport/auth
    # options are meaningful, and profile references are rejected above.
    equivalent_settings = {
        "base_url": api["base_url"], "wire_api": "responses",
        "requires_openai_auth": True, "supports_websockets": api["supports_websockets"],
    }
    duplicate_ids = {
        identifier for identifier, settings in providers.items()
        if identifier != PROVIDER and re.fullmatch(r"[A-Za-z0-9_-]+", identifier)
        and isinstance(settings, dict)
        and {key: value for key, value in settings.items() if key != "name"} == equivalent_settings
    }
    removed_sections = {f"model_providers.{identifier}" for identifier in duplicate_ids}
    previous_model = parsed.get("model")
    selected_model = next((model for model in models if model["slug"] == previous_model), models[0])
    selected = selected_model["slug"]
    reasoning = parsed.get("model_reasoning_effort")
    managed_keys = set(_OWNED_KEYS)
    reasoning_prefix = ""
    if reasoning is not None:
        supported = {level["effort"] for level in selected_model["supported_reasoning_levels"]}
        if previous_model != selected or not isinstance(reasoning, str) or reasoning not in supported:
            reasoning = selected_model["default_reasoning_level"]
            managed_keys.add("model_reasoning_effort")
            reasoning_prefix = f'model_reasoning_effort = {json.dumps(reasoning)}\n'

    lines = original.splitlines(keepends=True)
    kept: list[str] = []
    in_root = True
    skip_provider = False
    for line in lines:
        if _ANY_HEADER.match(line):
            header = _HEADER.match(line)
            name = header.group(1).strip() if header else ""
            if name.startswith(f"model_providers.{PROVIDER}.") or name == f"model_providers.{PROVIDER}":
                if name != f"model_providers.{PROVIDER}":
                    raise ValueError("nested cockpit_remote provider tables require manual review")
                skip_provider = True
            else:
                skip_provider = name in removed_sections
            in_root = False
        if skip_provider:
            continue
        if in_root:
            match = _KEY.match(line)
            if match and match.group(1) == "model_providers":
                raise ValueError("inline model_providers table requires manual review")
            if match and match.group(1) in managed_keys:
                continue
        kept.append(line)
    prefix = (
        f'model = {json.dumps(selected)}\n'
        f'model_provider = {json.dumps(PROVIDER)}\n'
        f'model_catalog_json = {json.dumps(str(home / CATALOG_NAME))}\n'
    )
    prefix += reasoning_prefix
    # Own the separator before the managed provider; do not accumulate old blank lines.
    while kept and not kept[-1].strip():
        kept.pop()
    body = "".join(kept)
    if body and not body.endswith("\n"):
        body += "\n"
    provider = (
        f"\n[model_providers.{PROVIDER}]\n"
        f'name = {json.dumps(api.get("provider_name", "Cockpit Remote"), ensure_ascii=False)}\n'
        f'base_url = {json.dumps(api["base_url"])}\n'
        'wire_api = "responses"\n'
        'requires_openai_auth = true\n'
        f'supports_websockets = {str(api["supports_websockets"]).lower()}\n'
    )
    merged = prefix + body + provider
    try:
        checked = tomllib.loads(merged)
    except tomllib.TOMLDecodeError as exc:
        raise ValueError("generated config.toml is invalid") from exc
    if checked["model"] != selected or checked["model_provider"] != PROVIDER:
        raise ValueError("generated config.toml did not select the provider")
    def unrelated(doc: dict) -> dict:
        result = copy.deepcopy(doc)
        for key in managed_keys:
            result.pop(key, None)
        provider_table = result.get("model_providers")
        if isinstance(provider_table, dict):
            for identifier in duplicate_ids | {PROVIDER}:
                provider_table.pop(identifier, None)
            if not provider_table:
                result.pop("model_providers")
        return result

    if unrelated(parsed) != unrelated(checked):
        raise ValueError("unsupported config syntax would change unrelated settings")
    return merged


def prepare_api_bundle(home: str | Path, api: dict) -> dict[str, bytes]:
    """Return config and catalog bytes; do not modify ``home``."""
    _require_python()
    root = Path(home).expanduser().resolve()
    overrides = _definition(api)
    official = _catalog(root, bundled=True)
    template = _automatic_template(official["models"])
    _validate_auto_overrides(overrides)
    template = copy.deepcopy(template)
    template["use_responses_lite"] = False
    models = _models(template, overrides)
    config_path = root / "config.toml"
    original = config_path.read_text(encoding="utf-8") if config_path.exists() else ""
    config = _config(original, api, models, root)
    catalog = json.dumps({"models": models}, ensure_ascii=False, separators=(",", ":")) + "\n"
    return {"config.toml": config.encode(), CATALOG_NAME: catalog.encode()}


def validate_api_bundle(home: str | Path, api: dict, prepared: dict[str, bytes]) -> None:
    """Check installed bundle against prepared bytes and the Codex effective catalog.

    Call this only after the caller has atomically installed the prepared files.
    """
    _require_python()
    root = Path(home).expanduser().resolve()
    _definition(api)
    if set(prepared) != {"config.toml", CATALOG_NAME} or any(type(v) is not bytes for v in prepared.values()):
        raise ValueError("prepared bundle must contain exactly two byte files")
    for name, content in prepared.items():
        if (root / name).read_bytes() != content:
            raise ValueError(f"installed {name} differs from prepared bundle")
    config = tomllib.loads(prepared["config.toml"].decode())
    expected = json.loads(prepared[CATALOG_NAME])
    slugs = [model["slug"] for model in expected["models"]]
    if (config.get("model_provider") != PROVIDER or config.get("model") not in slugs
            or config.get("model_catalog_json") != str(root / CATALOG_NAME)):
        raise ValueError("installed config does not select the prepared catalog")
    provider = config.get("model_providers", {}).get(PROVIDER, {})
    if any(provider.get(key) != value for key, value in {
        "name": api.get("provider_name", "Cockpit Remote"), "base_url": api["base_url"], "wire_api": "responses",
        "requires_openai_auth": True, "supports_websockets": api["supports_websockets"],
    }.items()):
        raise ValueError("installed provider differs from API definition")
    actual = _catalog(root, bundled=False)
    actual_models = actual["models"]
    for model in expected["models"]:
        matches = [entry for entry in actual_models if isinstance(entry, dict) and entry.get("slug") == model["slug"]]
        if len(matches) != 1:
            raise ValueError(f"Codex effective catalog differs for {model['slug']}: expected one entry, found {len(matches)}")
        if matches[0] != model:
            actual_model = matches[0]
            fields = sorted(key for key in model.keys() | actual_model.keys()
                            if key not in model or key not in actual_model or model[key] != actual_model[key])
            raise ValueError(f"Codex effective catalog differs for {model['slug']}: fields {', '.join(fields)}")
