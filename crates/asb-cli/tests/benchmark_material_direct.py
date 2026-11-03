"""Direct creation receipts and shared bridge scope without real Docker."""
import copy
from pathlib import Path
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0,str(Path(__file__).parents[1]/"assets/benchmarks"))
from asb_material import MaterialError, capture_task
from asb_material_declarations import read
from asb_material_direct import declare
from asb_material_networks import observe
from asb_material_network_identity import canonicalize
from asb_resources import ResourceJournal, RecoveryError
from benchmark_resources import client,Container,ATTEMPT


class ForeignMembers(dict):
    def __iter__(self):
        raise AssertionError("foreign membership must not be enumerated")
    def items(self):
        raise AssertionError("foreign membership must not be copied")
    def __deepcopy__(self,memo):
        raise AssertionError("foreign membership must not be copied")


class DirectTests(unittest.TestCase):
    def test_filesystem_capture_profile_is_declared_and_other_mount_profiles_are_unavailable(self):
        task, _, options = self.fixture()
        options.update(devices=['/dev/fuse:/dev/fuse:rwm'], cap_add=['SYS_ADMIN'],
                       security_opt=['apparmor=unconfined'])
        self.assertTrue(declare(options,task.journal.record)['available'])
        for key in ('devices','cap_add','security_opt'):
            changed = copy.deepcopy(options)
            changed.pop(key)
            self.assertFalse(declare(changed,task.journal.record)['available'])

    def fixture(self,mode=None):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        api = client()
        journal = ResourceJournal(api,Path(temporary.name),ATTEMPT)
        self.addCleanup(journal.close)
        options = {"image":"sha256:"+"d"*64,"name":ATTEMPT,"detach":True,
            "command":["sleep","infinity"],"working_dir":"/task",
            "labels":journal.declare("containers",ATTEMPT)}
        if mode is not None:
            options["network_mode"] = mode
        journal.direct_container(options)
        resource = Container(ATTEMPT,journal.labels)
        resource.id = "1"*64
        resource.attrs.update(Id=resource.id,HostConfig={"NetworkMode":mode or "default"})
        resource.attrs["Config"].update(Hostname=resource.id[:12],WorkingDir="/task")
        endpoint = {"NetworkID":"a"*64,"EndpointID":"b"*64,"IPAddress":"192.0.2.2","IPPrefixLen":24,
            "Gateway":"192.0.2.1","GlobalIPv6Address":"","GlobalIPv6PrefixLen":0,
            "MacAddress":"02:42:c0:00:02:02","Aliases":None,"IPAMConfig":None}
        resource.attrs["NetworkSettings"] = {"Networks":{"bridge":endpoint} if mode != "none" else {"none":{}}}
        api.containers.values[ATTEMPT] = resource
        members = ForeignMembers({resource.id:{"Name":ATTEMPT,"EndpointID":"b"*64,
            "IPv4Address":"192.0.2.2/24","IPv6Address":"","MacAddress":endpoint["MacAddress"]}})
        network = SimpleNamespace(id="a"*64,name="bridge",reload=lambda:None,attrs={
            "Id":"a"*64,"Name":"bridge","Driver":"bridge","Scope":"local","Internal":False,
            "Options":{"com.docker.network.bridge.default_bridge":"true","com.docker.network.bridge.name":"docker0"},
            "IPAM":{"Driver":"default","Config":[{"Subnet":"192.0.2.0/24","Gateway":"192.0.2.1"}]},
            "Containers":members})
        api.networks.values["bridge"] = network
        task = SimpleNamespace(client=api,journal=journal,cancelled=threading.Event(),
            workspace=SimpleNamespace(container=resource,workdir="/task"))
        return task,network,options

    def observe(self,task):
        return observe(task,time.monotonic()+3)

    def test_direct_intent_is_frozen_durable_and_never_declares_global_bridge(self):
        task,_,options = self.fixture()
        receipt = read(task.journal,"material_services","material-services.json")
        self.assertEqual(receipt["networks"],{})
        self.assertEqual(receipt["external_networks"]["default"]["name"],"bridge")
        self.assertEqual(task.journal.record["resources"],[{"kind":"containers","name":ATTEMPT}])
        self.assertNotIn("labels",receipt)
        task.journal.direct_container(copy.deepcopy(options))
        options["command"].append("changed")
        with self.assertRaises(RecoveryError):
            task.journal.direct_container(options)
        self.assertEqual(read(task.journal,"material_services","material-services.json"),receipt)

    def test_shared_bridge_ignores_foreign_churn_but_binds_own_endpoint_and_literal_config(self):
        task,network,_ = self.fixture()
        first = self.observe(task)
        public,mapping = canonicalize(first)
        self.assertNotIn("Containers",first["networks"]["default"]["attributes"])
        network.attrs["Containers"]["f"*64] = {"Name":"unrelated-secret"}
        self.assertEqual(first,self.observe(task))
        self.assertEqual(task.client.networks.lookups,["bridge","bridge"])
        selected = public["networks"]["default"]
        self.assertEqual(selected["ownership"],"external-shared-default-bridge-v1")
        self.assertEqual(selected["configuration"]["IPAM"]["Config"][0]["Subnet"],"192.0.2.0/24")
        self.assertEqual(selected["configuration"]["Options"]["com.docker.network.bridge.name"],"docker0")
        self.assertEqual(selected["endpoints"]["main"]["NetworkID"],{"shared_network":"default"})
        self.assertEqual(public["modes"]["main"],{"shared_network":"default"})
        self.assertEqual(mapping["main"]["192.0.2.2"]["automatic_address"],"main")
        network.attrs["Options"]["com.docker.network.bridge.enable_icc"] = "false"
        self.assertNotEqual(public,canonicalize(self.observe(task))[0])

    def test_own_endpoint_disappearance_replacement_address_and_wrong_bridge_are_refused(self):
        for kind in ["missing","endpoint","address","prefix","not-default","external-driver"]:
            task,network,_ = self.fixture()
            member = network.attrs["Containers"]["1"*64]
            if kind == "missing":
                del network.attrs["Containers"]["1"*64]
            elif kind == "endpoint":
                member["EndpointID"] = "e"*64
            elif kind == "address":
                member["IPv4Address"] = "192.0.2.99/24"
            elif kind == "prefix":
                member["IPv4Address"] = "192.0.2.2/25"
            elif kind == "not-default":
                network.attrs["Options"].clear()
            else:
                network.attrs["Driver"] = "overlay"
            with self.subTest(kind=kind),self.assertRaises(MaterialError):
                self.observe(task)

    def test_shared_ipv6_flags_and_default_ipam_options_are_external_literals(self):
        task,network,_ = self.fixture()
        network.attrs["EnableIPv6"] = True
        network.attrs["Attachable"] = True
        network.attrs["IPAM"]["Options"] = {"observed-default-option":"preserve"}
        network.attrs["IPAM"]["Config"].append({"Subnet":"2001:db8::/64","Gateway":"2001:db8::1"})
        endpoint = task.workspace.container.attrs["NetworkSettings"]["Networks"]["bridge"]
        endpoint.update(GlobalIPv6Address="2001:db8::2",GlobalIPv6PrefixLen=64,IPv6Gateway="2001:db8::1")
        network.attrs["Containers"]["1"*64]["IPv6Address"] = "2001:db8::2/64"
        public,_ = canonicalize(self.observe(task))
        config = public["networks"]["default"]["configuration"]
        self.assertTrue(config["EnableIPv6"])
        self.assertTrue(config["Attachable"])
        self.assertEqual(config["IPAM"]["Options"],{"observed-default-option":"preserve"})
        self.assertEqual(config["IPAM"]["Config"][1]["Subnet"],"2001:db8::/64")
        network.attrs["Attachable"] = False
        self.assertNotEqual(public,canonicalize(self.observe(task))[0])

    def test_shared_aggregate_allocation_counters_are_not_owned_endpoint_state(self):
        task,network,_ = self.fixture()
        baseline = self.observe(task)
        network.attrs["Status"] = ForeignMembers({"IPAM":{"Subnets":{
            "192.0.2.0/24":{"IPsInUse":55,"DynamicIPsAvailable":201}}}})
        self.assertEqual(baseline,self.observe(task))

    def test_private_create_digest_and_attempt_names_do_not_enter_public_identity(self):
        task,_,_ = self.fixture()
        first = self.observe(task)
        changed = copy.deepcopy(first)
        changed["services"]["main"] = "2"*64
        changed["service_intent"]["main"]["container"] = "new-owned-name"
        endpoint = changed["networks"]["default"]["endpoints"]["main"]
        endpoint.update(EndpointID="e"*64,IPAddress="192.0.2.3",MacAddress="02:42:c0:00:02:03")
        self.assertEqual(canonicalize(first)[0],canonicalize(changed)[0])
        self.assertNotIn("creation_sha256",str(canonicalize(first)[0]))

    def test_single_network_none_dispatches_composer_and_verified_missing_receipt_never_falls_back(self):
        task,_,_ = self.fixture(mode="none")
        expected = {"schema_version":1,"scope":"prepared-workload-material-v1","sha256":"a"*64}
        with patch("asb_material_environment.capture",return_value=expected) as compose, \
             patch("asb_material_root.capture") as root:
            self.assertEqual(capture_task(task,{"fixture":"none"},binary="/not-started"),expected)
            compose.assert_called_once()
            root.assert_not_called()
        del task.journal.material_services
        with self.assertRaisesRegex(MaterialError,"pre-creation"):
            capture_task(task,{"fixture":"none"},binary="/not-started")

    def test_unsupported_creation_profiles_and_receipt_mutation_are_not_reauthorized(self):
        task,_,options = self.fixture()
        for key,value in [("network_mode","host"),("unknown","unsupported")]:
            changed = dict(options)
            changed[key] = value
            self.assertFalse(declare(changed,task.journal.record)["available"])
        task.journal.material_services["external_networks"]["default"]["name"] = "foreign"
        with self.assertRaises(MaterialError):
            self.observe(task)


if __name__ == "__main__":
    unittest.main()
