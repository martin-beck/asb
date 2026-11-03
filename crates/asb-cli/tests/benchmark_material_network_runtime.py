"""Network/runtime provenance and exact generated-file transformations."""
import copy
from pathlib import Path
import sys
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0,str(Path(__file__).parents[1]/"assets/benchmarks"))
from asb_material import MaterialError
from asb_material_network_identity import canonicalize
from asb_material_network_runtime import validate_sandboxes
import asb_material_runtime as runtime
import benchmark_material_networks as fixtures
from benchmark_material_environment import files


class RuntimeTests(unittest.TestCase):
    fixture = fixtures.NetworkTests.fixture
    observe = fixtures.NetworkTests.observe

    def test_declared_filesystem_capture_accepts_only_the_observed_profile(self):
        attrs = {"Config":{},"HostConfig":{"NetworkMode":"none",
            "Devices":[{"PathOnHost":"/dev/fuse","PathInContainer":"/dev/fuse","CgroupPermissions":"rwm"}],
            "CapAdd":["CAP_SYS_ADMIN"],"SecurityOpt":["apparmor=unconfined"]},
            "NetworkSettings":{"Networks":{"none":{}}}}
        declaration = {"filesystem_capture":True}
        result = runtime.configuration(attrs,"main",declaration)
        self.assertEqual(result["host"]["Devices"],attrs["HostConfig"]["Devices"])
        with self.assertRaises(MaterialError):
            runtime.configuration(attrs,"main",{})
        for key,value in [("Devices",[]),("CapAdd",[]),("SecurityOpt",[])]:
            changed = copy.deepcopy(attrs)
            changed["HostConfig"][key] = value
            with self.subTest(key=key),self.assertRaises(MaterialError):
                runtime.configuration(changed,"main",declaration)

    def network(self):
        task,network = self.fixture()
        for resource in task.client.containers.values.values():
            resource.attrs["Id"] = resource.id
            resource.attrs["Config"].update(Hostname=resource.id[:12],Domainname="",Cmd=["sleep","infinity"])
            resource.attrs["NetworkSettings"]["Networks"][network.name]["Gateway"] = "192.0.2.1"
            resource.attrs["NetworkSettings"].update(SandboxID=resource.id,
                SandboxKey="/var/run/docker/netns/"+resource.id[:12])
        raw = self.observe(task)
        canonical,addresses = canonicalize(raw)
        return task.workspace.container.attrs,raw,canonical,addresses

    def test_runtime_binds_owned_mode_endpoints_sandbox_and_retains_other_configuration(self):
        attrs,raw,canonical,_ = self.network()
        attrs["NetworkSettings"].update(SandboxID="d"*64,SandboxKey="/var/run/docker/netns/"+"d"*12,
            Ports={"80/tcp":None},HairpinMode=False,IPAddress="192.0.2.2",IPPrefixLen=24)
        attrs["Config"]["Env"] = ["PRESERVE=literal"]
        result = runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))
        self.assertEqual(result["host"]["NetworkMode"],{"owned_network":"private"})
        self.assertEqual(result["network"]["SandboxID"],{"owned_sandbox":"main"})
        self.assertEqual(result["network"]["Networks"]["private"],canonical["networks"]["private"]["endpoints"]["main"])
        self.assertEqual(result["config"]["Env"],["PRESERVE=literal"])
        self.assertEqual(result["network"]["Ports"],{"80/tcp":None})
        self.assertEqual(result["network"]["IPAddress"],{"automatic_address":"main","network":"private","family":4})

    def test_foreign_sandbox_legacy_addresses_ports_and_service_replacement_are_refused(self):
        mutations = [lambda attrs:attrs.update(Id="f"*64),
            lambda attrs:attrs["HostConfig"].update(NetworkMode="foreign"),
            lambda attrs:attrs["NetworkSettings"].update(SandboxID="d"*64,SandboxKey="/foreign/sandbox"),
            lambda attrs:attrs["NetworkSettings"].update(SandboxKey="/foreign/sandbox"),
            lambda attrs:attrs["NetworkSettings"].update(IPAddress="198.51.100.99"),
            lambda attrs:attrs["NetworkSettings"].update(EndpointID="f"*64),
            lambda attrs:attrs["NetworkSettings"].update(Ports={"80/tcp":[{"HostPort":"8080"}]}),
            lambda attrs:attrs["NetworkSettings"].update(SecondaryIPAddresses=["192.0.2.99"])]
        for mutate in mutations:
            attrs,raw,canonical,_ = self.network()
            mutate(attrs)
            with self.subTest(mutation=mutate),self.assertRaises(MaterialError):
                runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))

    def test_only_own_generated_host_line_gets_a_typed_address(self):
        attrs,raw,_,addresses = self.network()
        hostname = attrs["Config"]["Hostname"]
        payload = files(hostname)
        payload[0]["text"] = "192.0.2.2\t"+hostname+" # keep comment\n192.0.2.2 literal-name\n# 192.0.2.2\n"
        normalized = runtime.normalize(payload,"main",attrs,raw["service_intent"]["main"],{},addresses["main"])
        lines = normalized[0]["content"]
        self.assertEqual(lines[0]["tokens"][0],{"automatic_address":"main","network":"private","family":4})
        self.assertEqual(lines[0]["comment"],"# keep comment\n")
        self.assertEqual(lines[1]["tokens"][0],{"literal":"192.0.2.2"})
        self.assertEqual(lines[2],{"literal":"# 192.0.2.2\n"})
        lines[0]["tokens"][0]["network"] = "caller-mutation"
        self.assertEqual(addresses["main"]["192.0.2.2"]["network"],"private")

    def test_explicit_host_alias_static_address_and_extra_host_collisions_stay_literal(self):
        for kind in ["extra-name","extra-address","static-other-network","explicit-hostname","explicit-alias"]:
            attrs,raw,_,addresses = self.network()
            hostname = attrs["Config"]["Hostname"]
            declaration = copy.deepcopy(raw["service_intent"]["main"])
            if kind == "extra-name":
                attrs["HostConfig"]["ExtraHosts"] = [hostname+":192.0.2.99"]
            elif kind == "extra-address":
                attrs["HostConfig"]["ExtraHosts"] = ["literal:192.0.2.2"]
            elif kind == "static-other-network":
                attrs["NetworkSettings"]["Networks"]["other"] = {"IPAMConfig":{"IPv4Address":"192.0.2.2"}}
            elif kind == "explicit-hostname":
                declaration["hostname"] = hostname
            else:
                declaration["networks"]["private"]["aliases"] = [hostname]
            payload = files(hostname)
            payload[0]["text"] = "192.0.2.2 "+hostname+"\n"
            result = runtime.normalize(payload,"main",attrs,declaration,{},addresses["main"])
            with self.subTest(kind=kind):
                self.assertEqual(result[0]["content"][0]["tokens"][0],{"literal":"192.0.2.2"})
                if kind == "extra-name":
                    self.assertEqual(result[0]["content"][0]["tokens"][2],{"literal":hostname})

    def test_wrong_endpoint_snapshot_and_unknown_runtime_fields_cannot_disappear(self):
        attrs,raw,canonical,_ = self.network()
        result = runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))
        attrs["NetworkSettings"]["future_setting"] = {"literal":"preserve"}
        changed = runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))
        self.assertNotEqual(result,changed)
        self.assertEqual(changed["network"]["future_setting"],{"literal":"preserve"})
        key = next(iter(attrs["NetworkSettings"]["Networks"]))
        attrs["NetworkSettings"]["Networks"][key]["EndpointID"] = "f"*64
        with self.assertRaises(MaterialError):
            runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))

    def test_legacy_fields_must_describe_one_endpoint_not_a_mixture(self):
        attrs,raw,canonical,_ = self.network()
        extra = copy.deepcopy(raw["networks"]["private"])
        extra["attributes"].update(Name="second-network",Id="f"*64)
        endpoint = extra["endpoints"]["main"]
        endpoint.update(NetworkID="f"*64,IPAddress="198.51.100.2",IPPrefixLen=25,
            EndpointID="e"*64,Gateway="198.51.100.1",MacAddress="02:00:00:00:00:ef")
        raw["networks"]["second"] = extra
        canonical["networks"]["second"] = copy.deepcopy(canonical["networks"]["private"])
        attrs["NetworkSettings"]["Networks"]["second-network"] = copy.deepcopy(endpoint)
        attrs["NetworkSettings"].update(IPAddress="192.0.2.2",IPPrefixLen=25,
            EndpointID="e"*64,MacAddress=endpoint["MacAddress"])
        with self.assertRaises(MaterialError):
            runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))

    def test_private_services_cannot_share_sandbox_identity_or_key(self):
        attrs,_,_,_ = self.network()
        other = copy.deepcopy(attrs)
        with self.assertRaisesRegex(MaterialError,"share"):
            validate_sandboxes({"main":attrs,"database":other})
        other["NetworkSettings"]["SandboxID"] = "e"*64
        with self.assertRaisesRegex(MaterialError,"share"):
            validate_sandboxes({"main":attrs,"database":other})
        other["NetworkSettings"]["SandboxKey"] = "/var/run/docker/netns/"+"e"*12
        validate_sandboxes({"main":attrs,"database":other})
        attrs,raw,canonical,_ = self.network()
        attrs["NetworkSettings"].update(SandboxID="",SandboxKey="")
        with self.assertRaises(MaterialError):
            runtime.configuration(attrs,"main",raw["service_intent"]["main"],(raw,canonical))

    def test_uncovered_topology_stops_before_instrumentation(self):
        import asb_material_environment as environment
        task,network = self.fixture()
        network.attrs["Driver"] = "overlay"
        with patch.object(environment,"prepare") as prepare:
            with self.assertRaises(MaterialError):
                environment.capture(task,{"fixture":"not-started"},[],"/not-started",time.monotonic()+3)
            prepare.assert_not_called()

    def test_single_routed_service_dispatch_cannot_fall_back_to_root_only(self):
        from asb_material import capture_task
        task,_ = self.fixture()
        receipt = task.journal.material_services
        expected = {"schema_version":1,"scope":"prepared-workload-material-v1","sha256":"a"*64}
        with patch("asb_material_services.resolve",return_value=(receipt,{"main":task.workspace.container},"main")), \
             patch("asb_material_environment.capture",return_value=expected) as compose, \
             patch("asb_material_root.capture") as root:
            self.assertEqual(capture_task(task,{"fixture":"single"},binary="/not-started"),expected)
            compose.assert_called_once()
            root.assert_not_called()


if __name__ == "__main__":
    unittest.main()
