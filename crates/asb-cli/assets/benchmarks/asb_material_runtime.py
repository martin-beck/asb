"""Private Docker-generated files and conservative inspected runtime semantics."""
import copy
import ipaddress
import json
import re

from asb_material import MaterialError, canonical_digest
from asb_material_exec import run

NAMES = ["hosts","hostname","resolv.conf"]


def read_files(command, budget, *, deadline, cancelled):
    limits = budget.allowance()
    output = run(command,json.dumps(limits).encode(),deadline=deadline,
        cancelled=cancelled,output_limit=65536)
    try:
        value = json.loads(output)
    except (ValueError,UnicodeError):
        raise MaterialError("runtime file helper returned invalid private JSON") from None
    if not isinstance(value,dict) or set(value) != {"files","usage"}:
        raise MaterialError("runtime file helper returned invalid observation fields")
    files = value["files"]
    fields = {"name","text","mode","uid","gid","inode_group","attributes_sha256"}
    if not isinstance(files,list) or len(files) != 3:
        raise MaterialError("runtime file helper returned incomplete coverage")
    for index,item in enumerate(files):
        if (not isinstance(item,dict) or set(item) != fields or item["name"] != NAMES[index]
                or not isinstance(item["text"],str) or len(item["text"]) > 4096
                or any(ord(char) not in [9,10,13] and not 32 <= ord(char) <= 126 for char in item["text"])
                or any(type(item[key]) is not int or not 0 <= item[key] <= maximum
                    for key,maximum in [("mode",0o7777),("uid",2**32-1),("gid",2**32-1)])
                or item["inode_group"] not in NAMES[:index+1]
                or not isinstance(item["attributes_sha256"],str)
                or not re.fullmatch(r"[0-9a-f]{64}",item["attributes_sha256"])):
            raise MaterialError("runtime file helper returned unsupported text or metadata")
    usage = value["usage"]
    if (not isinstance(usage,dict) or usage.get("entries") != 3 or usage.get("names") != 39
            or usage.get("bytes") != sum(len(item["text"]) for item in files)
            or type(usage.get("attributes")) is not int or not 0 <= usage["attributes"] <= 65536):
        raise MaterialError("runtime file helper returned contradictory consumption")
    budget.consume(usage,limits)
    return files


def generated_hostname(attributes, declaration):
    value = attributes.get("Config",{}).get("Hostname")
    identity = attributes.get("Id")
    return (declaration.get("hostname") is None and isinstance(identity,str)
        and isinstance(value,str) and value == identity[:12])


def normalize(files, role, attributes, declaration, role_names, addresses=None):
    """Tag exact generated hostname tokens; preserve literal text and comments."""
    generated = generated_hostname(attributes,declaration)
    hostname = attributes.get("Config",{}).get("Hostname")
    aliases = {name:{"owned_container":owner} for name,owner in role_names.items()}
    protected_names,protected_addresses = set(),set()
    for item in attributes.get("HostConfig",{}).get("ExtraHosts") or []:
        if not isinstance(item,str):
            raise MaterialError("prepared explicit host mapping is invalid")
        name,separator,address = item.partition(":")
        protected_names.add(name)
        if separator:
            try:
                protected_addresses.add(str(ipaddress.ip_address(address)))
            except ValueError:
                pass
    for endpoint in attributes.get("NetworkSettings",{}).get("Networks",{}).values():
        for key in ["IPv4Address","IPv6Address"]:
            address = (endpoint.get("IPAMConfig") or {}).get(key)
            if address:
                protected_addresses.add(str(ipaddress.ip_address(address)))
    intent = declaration.get("networks")
    if isinstance(intent,dict):
        for attachment in intent.values():
            protected_names.update((attachment or {}).get("aliases") or [])
    if generated:
        aliases[hostname] = {"generated_hostname":role}
        domain = attributes.get("Config",{}).get("Domainname")
        if domain:
            aliases[hostname+"."+domain] = {"generated_hostname":role,"domain":domain}
    else:
        aliases.pop(hostname,None)
    for name in protected_names:
        aliases.pop(name,None)
    own_names = {hostname}
    domain = attributes.get("Config",{}).get("Domainname")
    if domain:
        own_names.add(hostname+"."+domain)
    result = []
    for item in files:
        value = copy.deepcopy(item)
        text = value.pop("text")
        if item["name"] == "hostname" and generated and text in [hostname,hostname+"\n",hostname+"\r\n"]:
            value["content"] = {"generated_hostname":role,"ending":text[len(hostname):]}
        elif item["name"] == "hosts":
            lines = []
            for line in text.splitlines(keepends=True):
                body,separator,comment = line.partition("#")
                words = body.split()
                try:
                    if not words:
                        raise ValueError()
                    ipaddress.ip_address(words[0])
                except ValueError:
                    lines.append({"literal":line})
                    continue
                tokens = re.split(r"([ \t\r\n]+)",body)
                first = next(index for index,token in enumerate(tokens) if token and not token.isspace())
                selected = [aliases.get(token,{"literal":token}) if index > first and not token.isspace()
                    else {"literal":token} for index,token in enumerate(tokens)]
                address = str(ipaddress.ip_address(words[0]))
                # Only Docker's own generated-hostname line is eligible. User
                # aliases, static endpoint IPs, ExtraHosts and arbitrary lines
                # are not rewritten even when an address happens to coincide.
                if (addresses and generated and address in addresses and address not in protected_addresses
                        and hostname in words[1:] and set(words[1:]) <= own_names
                        and not set(words[1:]) & protected_names):
                    selected[first] = copy.deepcopy(addresses[address])
                lines.append({"tokens":selected,"comment":separator+comment})
            value["content"] = lines
        else:
            value["content"] = {"literal":text}
        result.append(value)
    return result


def configuration(attributes, role, declaration, network=None):
    """Retain inspected runtime fields; refuse uncovered external dependencies."""
    config,host = attributes.get("Config"),attributes.get("HostConfig")
    if (not isinstance(config,dict) or not isinstance(host,dict)
            or (network is None and host.get("NetworkMode") != "none")):
        raise MaterialError("prepared environment network mode needs explicit topology coverage")
    for key in ["PidMode","IpcMode","UTSMode","UsernsMode","CgroupnsMode"]:
        if host.get(key) not in [None,"","private"]:
            raise MaterialError("prepared environment shares an uncovered external namespace")
    devices = host.get("Devices") or []
    fuse_device = [{"PathOnHost":"/dev/fuse","PathInContainer":"/dev/fuse","CgroupPermissions":"rwm"}]
    if declaration.get("filesystem_capture") is True:
        if (devices != fuse_device or host.get("CapAdd") != ["CAP_SYS_ADMIN"]
                or host.get("SecurityOpt") != ["apparmor=unconfined"]):
            raise MaterialError("prepared filesystem recorder container differs from its creation profile")
    elif devices:
        raise MaterialError("prepared environment has uncovered external devices, ports or dependencies")
    for key in ["Privileged","DeviceRequests","DeviceCgroupRules","VolumesFrom","Links",
                "PortBindings","PublishAllPorts","ContainerIDFile"]:
        if host.get(key):
            raise MaterialError("prepared environment has uncovered external devices, ports or dependencies")
    networks = attributes.get("NetworkSettings",{}).get("Networks")
    if not isinstance(networks,dict) or (network is None and set(networks)-{"none"}):
        raise MaterialError("prepared environment has undeclared network endpoints")
    for endpoint in networks.values() if network is None else []:
        if not isinstance(endpoint,dict) or any(endpoint.get(key) for key in [
                "IPAddress","GlobalIPv6Address","Gateway","IPv6Gateway"]):
            raise MaterialError("prepared network-none environment unexpectedly has routed addresses")
    selected = copy.deepcopy(config)
    for key in ["Image","Labels"]:
        selected.pop(key,None)
    if generated_hostname(attributes,declaration):
        selected["Hostname"] = {"generated_hostname":role}
    options = copy.deepcopy(host)
    for key in ["Binds","Mounts","Tmpfs"]:
        options.pop(key,None)
    value = {"config":selected,"host":options,"network":{"mode":"none"}}
    if network is not None:
        from asb_material_network_runtime import integrate
        value = integrate(value,attributes,role,network)
    canonical_digest(value,"asb-private-runtime-configuration-v1")
    return value
