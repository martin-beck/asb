import tempfile
from pathlib import Path
import unittest
import faults


class Selection(unittest.TestCase):
    def test_only_owned_inherited_pid_recordings_are_eligible(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)/'project'
            target = project/'results/pressure-runs/run-001/measurements-concurrency-1/arm-spe.data'
            command = ['perf','record','--inherit','-p','42','-o',str(target)]
            self.assertEqual(faults.recording(command,project),target)
            variants = [
                ['perf','stat','--inherit','-p','42','-o',str(target)],
                command+['-a'],command+['--cpu=0'],command+['-C0'],command+['--no-inherit'],
                command+['--per-thread'],command+['-p','1'],command+['-o',str(target)],
                ['perf','record','--inherit','-p','-1','-o',str(target)],
                ['perf','record','--inherit','-p','42','-o'],
                ['perf','record','--inherit','-p','42','-o',str(target.with_name('perf.data'))],
                ['perf','record','--inherit','-p','42','-o',str(Path(directory)/'other/arm-spe.data')],
                ['perf','record','-p','42','-o',str(target)],
            ]
            for invalid in variants:
                self.assertIsNone(faults.recording(invalid,project),invalid)
            project.mkdir()
            (project/'results').symlink_to(Path(directory)/'outside',target_is_directory=True)
            self.assertIsNone(faults.recording(command,project))

    def test_loss_validation_rejects_failed_grades_and_fabricated_zero_samples(self):
        import json
        with tempfile.TemporaryDirectory() as directory:
            project=Path(directory)
            for replica in [1,2]:
                measured=project/('results/pressure-runs/run-001/measurements-concurrency-%d'%replica)
                measured.mkdir(parents=True)
                (measured/'run.json').write_text(json.dumps({'success_rate':1}))
                (measured/'arm-spe.json').write_text(json.dumps({'status':'failed' if replica==1 else 'completed','metrics':{}}))
                proof={'iterations':1000000000,'children':3,'io_blocks':128,'io_block_bytes':1024*1024,'checksums':['42']*3}
                (project/('pressure-concurrency-%d.log'%replica)).write_text(json.dumps({'stdout':'ASB_FIXED_WORK='+json.dumps(proof)}))
            measured=project/'results/pressure-runs/run-001/measurements-concurrency-1'
            analysis={'sampling':[{'metrics':[{'value':None}]}],'phase_alignments':[{'status':'unavailable'}]}
            (measured/'bottlenecks.json').write_text(json.dumps(analysis))
            faults.validate_loss(project,measured/'arm-spe.data')
            (measured/'run.json').write_text(json.dumps({'success_rate':0}))
            with self.assertRaises(RuntimeError):faults.validate_loss(project,measured/'arm-spe.data')
            (measured/'run.json').write_text(json.dumps({'success_rate':1}))
            analysis['sampling'][0]['metrics'][0]['value']=0
            (measured/'bottlenecks.json').write_text(json.dumps(analysis))
            with self.assertRaises(RuntimeError):faults.validate_loss(project,measured/'arm-spe.data')
