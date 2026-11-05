"""Offline fixture policy tests; no processes, privilege or hardware probes."""
import json
from pathlib import Path
import tempfile
import unittest
import overhead


class Policies(unittest.TestCase):
    def test_summary_keeps_units_and_reports_descriptive_change(self):
        rows = [dict(profile=profile, agent_cpu_ms_sum=v, agent_phase_span_upper_ms=v*2, cli_seconds=v/1000,service_wall_seconds=v/1000+1)
                for profile, values in [('light',[100,200,300]),('pmu-perf',[200,300,400]),('spe',[300,400,500])]
                for v in values]
        summary = overhead.summarize(rows)
        self.assertEqual(summary['light']['agent_cpu_ms_sum']['median'],200)
        self.assertAlmostEqual(summary['pmu-perf']['cli_seconds']['descriptive_change_from_light_pct'],50)
        self.assertEqual(summary['spe']['agent_phase_span_upper_ms']['min'],600)
        with self.assertRaises(ValueError): overhead.summarize(rows[:-3])
        rows[0]['cli_seconds'] = float('nan')
        with self.assertRaises(ValueError): overhead.summarize(rows)

    def test_staging_never_replaces_tools_or_prior_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)/'base'
            (base/'tools/perf').mkdir(parents=True)
            (base/'workloads').mkdir()
            (base/'workloads/bug-fix.txt').write_text('fixture')
            project = Path(directory)/'new'
            fingerprint = overhead.stage(base,project,Path('/usr/bin/node'),123,'light')
            self.assertEqual(len(fingerprint),64)
            self.assertIn('Number(123)',(project/'tools/pi/bin/pi').read_text())
            self.assertIn('kernel-pressure',(project/'pressure.toml').read_text())
            self.assertNotIn('arm-spe',(project/'pressure.toml').read_text())
            self.assertTrue((project/'tools/perf').is_symlink())
            with self.assertRaises(FileExistsError): overhead.stage(base,project,Path('/usr/bin/node'),999,'spe')
            self.assertIn('Number(123)',(project/'tools/pi/bin/pi').read_text())

    def test_extract_requires_both_owned_replicas_and_actual_fixed_work(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            def write(path,value):
                path.parent.mkdir(parents=True,exist_ok=True)
                path.write_text(json.dumps(value))
            scope = {'runner_pid':1,'fixture_pid':2,'noise_pid':3,'runner_wall_ns':6500000000,'observations':[{'leaves':{'a':[101],'b':[102]}}]}
            write(project/'qualification-scope.json',scope)
            for index in [1,2]:
                root = {'pid':100+index,'start_time_ticks':10}
                d = project/('results/pressure-runs/run-001/measurements-concurrency-%d'%index)
                write(d/'run.json',{'success_rate':1,'elapsed_ms':2000})
                write(d/'measurement-target.json',{'root':root,'excluded_guardian':{'pid':10+index},'excluded_controller_pid':1})
                write(d/'cpu-accounting.json',{'status':'completed','source':'cgroup-v2','metrics':{'cpu_user_ms':2000,'cpu_system_ms':100}})
                proof = {'iterations':123,'children':3,'io_blocks':128,'io_block_bytes':1024*1024,'checksums':['-42']*3}
                write(project/('pressure-concurrency-%d.log'%index),{'stdout':'ASB_FIXED_WORK='+json.dumps(proof)+'\n'})
            write(project/'results/pressure-comparison/cohort-analysis.json',{'cohorts':[{'status':'observed','peak_live_principals_lower_bound':2,'agent_phase_span_upper_bound_ms':2500}]})
            result = overhead.extract(project,'light',1,7.0,123)
            self.assertEqual(result['agent_cpu_ms_sum'],4200)
            self.assertEqual(result['cli_seconds'],6.5)
            self.assertEqual(result['service_wall_seconds'],7.0)
            scope['observations'][0]['leaves']['a'].append(1)
            write(project/'qualification-scope.json',scope)
            with self.assertRaises(ValueError): overhead.extract(project,'light',1,7.0,123)
            scope['observations'][0]['leaves']['a'].remove(1)
            write(project/'qualification-scope.json',scope)
            with self.assertRaises(ValueError): overhead.extract(project,'light',1,7.0,999)


if __name__ == '__main__': unittest.main()
