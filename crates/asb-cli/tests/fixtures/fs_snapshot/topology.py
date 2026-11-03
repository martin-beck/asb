"""Experimental hard-link split planner for snapshots of the same filesystem.

Inode numbers must identify the same original object across the two source
snapshots. This module does not implement rename handling or general replay.
"""
from collections import defaultdict
import os
from pathlib import Path
import stat


def regular_files(root):
    root = Path(root)
    root_device = root.stat().st_dev
    result = {}
    def fail(error):
        raise error
    for directory, names, files in os.walk(root, followlinks=False, onerror=fail):
        if os.lstat(directory).st_dev != root_device:
            raise ValueError('nested filesystem requires separate snapshot handling')
        names.sort(); files.sort()
        for name in files:
            path = Path(directory) / name
            metadata = os.lstat(path)
            if metadata.st_dev != root_device:
                raise ValueError('nested filesystem requires separate snapshot handling')
            if stat.S_ISREG(metadata.st_mode):
                result[path.relative_to(root).as_posix()] = (metadata.st_dev, metadata.st_ino)
    return result


def plan_splits(before, after, replica):
    """Return paths that must be detached before an in-place sync.

    Source identity is the inode number: these two inventories are snapshots
    of the same Btrfs filesystem, whose subvolumes have distinct device IDs.
    Replica identities are local (device, inode) pairs. A rename without an
    unchanged path anchor needs a separate identity-preserving operation.
    """
    if set(before) != set(replica):
        raise ValueError('replica regular-file paths differ from the previous snapshot')
    source_to_replica, replica_to_source = {}, {}
    old_paths, new_paths = defaultdict(set), defaultdict(set)
    for path, identity in before.items():
        source_id = identity[1]; replica_id = replica[path]
        if (source_id in source_to_replica and source_to_replica[source_id] != replica_id
                or replica_id in replica_to_source and replica_to_source[replica_id] != source_id):
            raise ValueError('replica hard-link topology differs from the previous snapshot')
        source_to_replica[source_id] = replica_id
        replica_to_source[replica_id] = source_id
        old_paths[source_id].add(path)
    for path, identity in after.items():
        new_paths[identity[1]].add(path)
    for source_id, paths in old_paths.items():
        if source_id in new_paths and not (paths & new_paths[source_id]):
            raise ValueError('snapshot inode move requires identity-preserving rename handling')
    return sorted(path for path in before.keys() & after.keys()
                  if before[path][1] != after[path][1])


def detach_splits(replica_root, paths):
    root = Path(replica_root)
    for path in paths:
        (root / path).unlink()
