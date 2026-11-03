"""Frozen direct Docker create intent; never authority over shared resources."""
import copy

from asb_material import MaterialError, canonical_digest
from asb_material_services import declare as services


def declare(options, record):
    identity = {key:record[key] for key in ["attempt","daemon","token"]}
    try:
        allowed = {"image","name","command","detach","working_dir","labels",
            "mem_limit","nano_cpus","pids_limit","hostname","mac_address","network_mode",
            "devices","cap_add","security_opt"}
        if not isinstance(options,dict) or set(options)-allowed or options.get("detach") is not True:
            raise MaterialError("direct container creation profile is unsupported")
        mount_options = {key:options[key] for key in ("devices","cap_add","security_opt") if key in options}
        if mount_options not in ({}, {"devices":["/dev/fuse:/dev/fuse:rwm"],
                "cap_add":["SYS_ADMIN"], "security_opt":["apparmor=unconfined"]}):
            raise MaterialError("direct container filesystem capture profile is unsupported")
        mode = options.get("network_mode")
        if mode not in [None,"default","bridge","none"]:
            raise MaterialError("direct container network profile is unsupported")
        service = {"container_name":options["name"],"hostname":options.get("hostname"),
            "mac_address":options.get("mac_address"),
            "networks":{"default":None} if mode != "none" else None}
        value = services({"services":{"main":service}},record)
        if value["available"] is not True:
            raise MaterialError("direct container role declaration is unavailable")
        if mount_options:
            value["services"]["main"]["filesystem_capture"] = True
        # Exact creation bytes are private provenance only. Their physical names,
        # labels and image references must never become baseline identity.
        value["creation_sha256"] = canonical_digest(copy.deepcopy(options),"asb-private-direct-create-v1")
        value["source"] = "direct-container-v1"
        value["external_networks"] = ({"default":{"name":"bridge",
            "profile":"external-shared-default-bridge-v1"}} if mode != "none" else {})
        return value
    except (MaterialError,KeyError,TypeError,ValueError):
        return {"schema_version":1,**identity,"available":False}
