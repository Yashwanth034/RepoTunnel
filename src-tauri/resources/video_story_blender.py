import bpy
import json
import math
import os
import wave
import struct
from pathlib import Path
from mathutils import Vector

base = Path(__file__).resolve().parent
data = json.loads((base / "shot.json").read_text(encoding="utf-8"))
bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene
scene.render.resolution_x = int(data["width"])
scene.render.resolution_y = int(data["height"])
scene.render.resolution_percentage = 75
scene.render.fps = min(int(data["fps"]), 12)
scene.frame_start = 1
scene.frame_end = max(1, int(round(float(data["duration"]) * scene.render.fps)))
scene.render.image_settings.file_format = "FFMPEG"
scene.render.ffmpeg.format = "MPEG4"
scene.render.ffmpeg.codec = "H264"
scene.render.ffmpeg.constant_rate_factor = "MEDIUM"
scene.render.ffmpeg.audio_codec = "NONE"
scene.render.filepath = data["output"]
try:
    scene.render.engine = "BLENDER_EEVEE_NEXT"
except Exception:
    try:
        scene.render.engine = "BLENDER_EEVEE"
    except Exception:
        pass

# Story animation is designed for CPU-friendly stylized rendering. The default
# 64-sample EEVEE budget is unnecessarily expensive for these flat/low-poly
# sets and makes long-form stories several hours slower without a useful
# visual gain. Keep this isolated to the story renderer; tutorial rendering is
# unchanged.
if hasattr(scene, "eevee"):
    if hasattr(scene.eevee, "taa_render_samples"):
        scene.eevee.taa_render_samples = 8
    if hasattr(scene.eevee, "taa_samples"):
        scene.eevee.taa_samples = 8

def safe_name(value):
    return "".join(ch if ch.isalnum() or ch in "-_." else "_" for ch in str(value))

def mat(name, rgba):
    material = bpy.data.materials.new(name)
    material.diffuse_color = rgba
    return material

actor_mats = [
    mat("actor-a", (0.18, 0.48, 0.88, 1.0)),
    mat("actor-b", (0.90, 0.42, 0.20, 1.0)),
    mat("actor-c", (0.35, 0.72, 0.42, 1.0)),
    mat("actor-d", (0.70, 0.38, 0.82, 1.0)),
]
prop_mat = mat("prop-default", (0.55, 0.38, 0.18, 1.0))
floor_mat = mat("floor", (0.12, 0.14, 0.18, 1.0))

def objects_after(before):
    before_ids = {obj.as_pointer() for obj in before}
    return [obj for obj in bpy.context.scene.objects if obj.as_pointer() not in before_ids]

def import_asset(path_text, label):
    if not path_text:
        return []
    path = Path(path_text)
    if not path.is_file():
        raise RuntimeError(f"{label} asset does not exist: {path}")
    ext = path.suffix.lower()
    before = list(bpy.context.scene.objects)
    if ext == ".blend":
        with bpy.data.libraries.load(str(path), link=False) as (src, dst):
            dst.objects = [name for name in src.objects if name]
        for obj in dst.objects:
            if obj is not None and obj.name not in scene.collection.objects:
                scene.collection.objects.link(obj)
    elif ext in (".glb", ".gltf"):
        bpy.ops.import_scene.gltf(filepath=str(path))
    elif ext == ".fbx":
        bpy.ops.import_scene.fbx(filepath=str(path))
    elif ext == ".obj":
        if hasattr(bpy.ops.wm, "obj_import"):
            bpy.ops.wm.obj_import(filepath=str(path))
        else:
            bpy.ops.import_scene.obj(filepath=str(path))
    elif ext in (".usd", ".usdc", ".usda"):
        bpy.ops.wm.usd_import(filepath=str(path))
    else:
        raise RuntimeError(
            f"{label} asset format '{ext}' is unsupported. Use .blend, .glb/.gltf, .fbx, .obj, or .usd/.usdc."
        )
    imported = objects_after(before)
    if not imported:
        raise RuntimeError(f"{label} asset imported no Blender objects: {path}")
    return imported

def create_root(name, objects):
    root = bpy.data.objects.new(name, None)
    scene.collection.objects.link(root)
    object_set = set(objects)
    for obj in objects:
        if obj.parent not in object_set:
            world = obj.matrix_world.copy()
            obj.parent = root
            obj.matrix_world = world
    return root

def descendants(root):
    out = []
    stack = list(root.children)
    while stack:
        obj = stack.pop()
        out.append(obj)
        stack.extend(list(obj.children))
    return out

def world_bounds(root):
    points = []
    for obj in descendants(root):
        if not hasattr(obj, "bound_box") or obj.type not in {"MESH", "CURVE", "SURFACE", "FONT", "META"}:
            continue
        for corner in obj.bound_box:
            points.append(obj.matrix_world @ Vector(corner))
    if not points:
        return None
    low = Vector((min(p.x for p in points), min(p.y for p in points), min(p.z for p in points)))
    high = Vector((max(p.x for p in points), max(p.y for p in points), max(p.z for p in points)))
    return low, high

def fit_root(root, target_height=None, target_span=None):
    bounds = world_bounds(root)
    if not bounds:
        return
    low, high = bounds
    size = high - low
    factor = 1.0
    if target_height and size.z > 0.0001:
        factor = min(factor, target_height / size.z)
    if target_span and max(size.x, size.y) > 0.0001:
        factor = min(factor, target_span / max(size.x, size.y))
    if factor < 0.999:
        root.scale *= factor

def normalized(name):
    return "".join(ch.lower() for ch in name if ch.isalnum())

def find_armature(root):
    for obj in [root] + descendants(root):
        if obj.type == "ARMATURE":
            return obj
    return None

BONES = {
    "left_hand": [
        "handl", "lefthand", "mixamoriglefthand", "hand_l", "lhand",
        "handfkl", "handikl", "defhandl",
    ],
    "right_hand": [
        "handr", "righthand", "mixamorigrighthand", "hand_r", "rhand",
        "handfkr", "handikr", "defhandr",
    ],
    "left_foot": [
        "footl", "leftfoot", "mixamorigleftfoot", "foot_l", "lfoot",
        "footfkl", "footikl", "deffootl",
    ],
    "right_foot": [
        "footr", "rightfoot", "mixamorgrightfoot", "foot_r", "rfoot",
        "footfkr", "footikr", "deffootr",
    ],
    "left_leg": [
        "calfl", "leftleg", "mixamorigleftleg", "shinl", "lowerlegl",
        "shinfkl", "shinikl", "defshinl",
    ],
    "right_leg": [
        "calfr", "rightleg", "mixamorigrightleg", "shinr", "lowerlegr",
        "shinfkr", "shinikr", "defshinr",
    ],
    "jaw": ["jaw", "mixamorijaw", "mixamorigjaw", "jawbone", "defjaw"],
    "head": ["head", "mixamorighead", "defhead"],
}

def find_pose_bone(armature, role):
    if not armature or not armature.pose:
        return None
    candidates = [normalized(v) for v in BONES.get(role, [])]
    for bone in armature.pose.bones:
        key = normalized(bone.name)
        if key in candidates or any(c in key for c in candidates):
            return bone
    return None

def anchor_offset(value):
    if not value:
        return 0.0
    total = sum(ord(ch) for ch in str(value))
    return ((total % 700) / 700.0 - 0.5) * 5.0

def make_proxy_actor(actor, index):
    root = bpy.data.objects.new(actor["id"] + "-proxy-root", None)
    scene.collection.objects.link(root)
    color = actor_mats[index % len(actor_mats)]
    def add_cube(name, loc, scale):
        bpy.ops.mesh.primitive_cube_add(size=1, location=loc)
        obj = bpy.context.object
        obj.name = name
        obj.scale = scale
        obj.data.materials.append(color)
        obj.parent = root
        return obj
    def add_sphere(name, loc, radius):
        bpy.ops.mesh.primitive_uv_sphere_add(segments=20, ring_count=12, radius=radius, location=loc)
        obj = bpy.context.object
        obj.name = name
        obj.data.materials.append(color)
        obj.parent = root
        return obj
    add_cube(actor["id"] + "-torso", (0, 0, 1.55), (0.55, 0.30, 0.80))
    add_sphere(actor["id"] + "-head", (0, 0, 2.75), 0.52)
    left_hand = add_sphere(actor["id"] + "-hand.L", (-0.95, 0, 1.55), 0.16)
    right_hand = add_sphere(actor["id"] + "-hand.R", (0.95, 0, 1.55), 0.16)
    add_cube(actor["id"] + "-leg.L", (-0.28, 0, 0.45), (0.18, 0.22, 0.65))
    add_cube(actor["id"] + "-leg.R", (0.28, 0, 0.45), (0.18, 0.22, 0.65))
    mouth = add_cube(actor["id"] + "-mouth", (0, -0.48, 2.62), (0.20, 0.03, 0.035))
    mouth["repotunnel_mouth"] = True
    root["repotunnel_left_hand"] = left_hand.name
    root["repotunnel_right_hand"] = right_hand.name
    return root

def make_proxy_prop(prop, index):
    root = bpy.data.objects.new(prop["id"] + "-prop-root", None)
    scene.collection.objects.link(root)
    bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, 0.45))
    obj = bpy.context.object
    obj.name = prop["id"] + "-prop"
    obj.scale = (0.42 + (index % 3) * 0.08, 0.30, 0.45)
    obj.data.materials.append(prop_mat)
    obj.parent = root
    return root

def add_location():
    asset = data.get("location", {}).get("assetPath")
    if asset:
        imported = import_asset(asset, "Location")
        root = create_root("LocationSet", imported)
        fit_root(root, target_span=24.0)
        return root
    bpy.ops.mesh.primitive_plane_add(size=24, location=(0, 0, 0))
    floor = bpy.context.object
    floor.name = "ProceduralFloor"
    floor.data.materials.append(floor_mat)
    return floor

location_root = add_location()

prop_roots = {}
for index, prop in enumerate(data.get("props", [])):
    asset = prop.get("assetPath")
    if asset:
        root = create_root(prop["id"] + "-prop-root", import_asset(asset, f"Prop {prop['id']}"))
        fit_root(root, target_height=1.4, target_span=1.8)
    else:
        root = make_proxy_prop(prop, index)
    root.location = (anchor_offset(prop.get("id")) * 0.55, 1.8 + (index % 3) * 0.8, 0.0)
    prop_roots[prop["id"]] = root

actor_roots = {}
actor_armatures = {}
actors = data.get("actors", [])
for index, actor in enumerate(actors):
    x = -4.5 + (9.0 * index / max(1, len(actors) - 1)) if len(actors) > 1 else 0.0
    asset = actor.get("assetPath")
    if asset:
        root = create_root(actor["id"] + "-root", import_asset(asset, f"Character {actor['id']}"))
        fit_root(root, target_height=3.6, target_span=2.4)
    else:
        root = make_proxy_actor(actor, index)
    root.location = (x, 0, 0)
    actor_roots[actor["id"]] = root
    actor_armatures[actor["id"]] = find_armature(root)

def keyframe_root_motion(actor, root, armature):
    action = str(actor.get("action") or "idle")
    start = scene.frame_start
    end = scene.frame_end
    start_location = root.location.copy()
    root.keyframe_insert(data_path="location", frame=start)
    shift = anchor_offset(actor.get("endAnchor"))
    if action in ("walk", "run"):
        root.location.x += shift if abs(shift) > 0.2 else (4.0 if action == "run" else 2.8)
        root.location.y += 1.4 if action == "run" else 0.8
    elif actor.get("endAnchor"):
        root.location.x += shift
    root.keyframe_insert(data_path="location", frame=end)

    if armature and action in ("walk", "run"):
        leg_l = find_pose_bone(armature, "left_leg")
        leg_r = find_pose_bone(armature, "right_leg")
        swing = math.radians(24 if action == "run" else 16)
        cycles = 6 if action == "run" else 4
        for step in range(cycles + 1):
            frame = int(round(start + (end - start) * step / max(1, cycles)))
            sign = -1.0 if step % 2 else 1.0
            for bone, angle in ((leg_l, swing * sign), (leg_r, -swing * sign)):
                if bone:
                    bone.rotation_mode = "XYZ"
                    bone.rotation_euler.x = angle
                    bone.keyframe_insert(data_path="rotation_euler", frame=frame)

def setup_foot_ik(actor, root, armature):
    action = str(actor.get("action") or "")
    if action not in ("walk", "run"):
        return
    if not armature:
        if actor.get("assetPath"):
            raise RuntimeError(
                f"Character '{actor['id']}' uses a real asset but has no armature for {action} foot contact."
            )
        return
    missing_feet = [
        role for role in ("left_foot", "right_foot")
        if find_pose_bone(armature, role) is None
    ]
    if actor.get("assetPath") and missing_feet:
        raise RuntimeError(
            f"Character '{actor['id']}' rig is missing required foot bones: {', '.join(missing_feet)}."
        )
    stride = 0.55 if action == "run" else 0.36
    lift = 0.22 if action == "run" else 0.12
    cycles = 6 if action == "run" else 4
    original_frame = scene.frame_current
    scene.frame_set(scene.frame_start)
    for role, phase in (("left_foot", 0), ("right_foot", 1)):
        foot = find_pose_bone(armature, role)
        if not foot:
            continue
        foot_world = armature.matrix_world @ foot.head
        root_world = root.matrix_world.translation.copy()
        base_offset = foot_world - root_world
        target = bpy.data.objects.new(f"{actor['id']}-{role}-ground-ik", None)
        scene.collection.objects.link(target)
        constraint = foot.constraints.new("IK")
        constraint.target = target
        constraint.chain_count = 2
        constraint.influence = 1.0
        for step in range(cycles + 1):
            frame = int(round(scene.frame_start + (scene.frame_end - scene.frame_start) * step / max(1, cycles)))
            scene.frame_set(frame)
            actor_world = root.matrix_world.translation.copy()
            moving = (step + phase) % 2 == 1
            position = actor_world + base_offset
            position.y += stride if moving else -stride * 0.25
            position.z = max(0.02, base_offset.z + (lift if moving else 0.0))
            target.location = position
            target.keyframe_insert(data_path="location", frame=frame)
    scene.frame_set(original_frame)

def target_position(target_id):
    if target_id in prop_roots:
        return prop_roots[target_id].matrix_world.translation.copy()
    if target_id in actor_roots:
        return actor_roots[target_id].matrix_world.translation.copy() + Vector((0, 0, 1.5))
    return None

def requested_hands(value):
    text = str(value or "").lower()
    if "both" in text or "two" in text:
        return ["left_hand", "right_hand"]
    if "left" in text:
        return ["left_hand"]
    if "right" in text:
        return ["right_hand"]
    return ["right_hand"]

def setup_interaction(actor, root, armature):
    target_id = actor.get("targetId")
    target = target_position(target_id)
    if not target:
        return
    action = str(actor.get("action") or "")
    hands = requested_hands(actor.get("handTarget"))
    if actor.get("assetPath") and not armature:
        raise RuntimeError(
            f"Character '{actor['id']}' uses a real asset but has no armature for hand-target interaction."
        )
    if armature:
        missing_hands = [role for role in hands if find_pose_bone(armature, role) is None]
        if actor.get("assetPath") and missing_hands:
            raise RuntimeError(
                f"Character '{actor['id']}' rig is missing required hand bones: {', '.join(missing_hands)}."
            )
        for offset_index, role in enumerate(hands):
            hand = find_pose_bone(armature, role)
            if not hand:
                continue
            empty = bpy.data.objects.new(f"{actor['id']}-{role}-ik", None)
            scene.collection.objects.link(empty)
            side = -0.16 if role == "left_hand" else 0.16
            empty.location = target + Vector((side, -0.08, 0.18 + offset_index * 0.05))
            constraint = hand.constraints.new("IK")
            constraint.target = empty
            constraint.chain_count = 2
            constraint.influence = 0.0
            constraint.keyframe_insert(data_path="influence", frame=scene.frame_start)
            constraint.influence = 1.0
            constraint.keyframe_insert(data_path="influence", frame=max(2, scene.frame_end // 3))
            constraint.keyframe_insert(data_path="influence", frame=scene.frame_end)

        if target_id in prop_roots and action in ("pick-up", "carry", "lift", "give", "receive"):
            hand = find_pose_bone(armature, hands[0])
            if hand:
                prop = prop_roots[target_id]
                con = prop.constraints.new("CHILD_OF")
                con.target = armature
                con.subtarget = hand.name
                con.influence = 0.0
                con.keyframe_insert(data_path="influence", frame=scene.frame_start)
                con.influence = 1.0
                con.keyframe_insert(data_path="influence", frame=max(2, scene.frame_end // 3))
                con.keyframe_insert(data_path="influence", frame=scene.frame_end)
    else:
        hand_name = root.get("repotunnel_right_hand")
        if hands and hands[0] == "left_hand":
            hand_name = root.get("repotunnel_left_hand")
        hand = bpy.data.objects.get(hand_name) if hand_name else None
        if hand:
            world_target = target
            local_target = root.matrix_world.inverted() @ world_target
            hand.keyframe_insert(data_path="location", frame=scene.frame_start)
            hand.location = local_target
            hand.keyframe_insert(data_path="location", frame=max(2, scene.frame_end // 3))
            hand.keyframe_insert(data_path="location", frame=scene.frame_end)

def pcm_rms(frames, width):
    if not frames:
        return 0.0
    if width == 1:
        samples = [value - 128 for value in frames]
    elif width == 2:
        usable = len(frames) - (len(frames) % 2)
        samples = [value[0] for value in struct.iter_unpack("<h", frames[:usable])]
    elif width == 3:
        samples = []
        for offset in range(0, len(frames) - 2, 3):
            value = int.from_bytes(frames[offset:offset + 3], "little", signed=False)
            if value & 0x800000:
                value -= 1 << 24
            samples.append(value)
    elif width == 4:
        usable = len(frames) - (len(frames) % 4)
        samples = [value[0] for value in struct.iter_unpack("<i", frames[:usable])]
    else:
        return 0.0
    if not samples:
        return 0.0
    return math.sqrt(sum(float(value) * float(value) for value in samples) / len(samples))

def mouth_amplitudes(path, duration):
    try:
        with wave.open(str(path), "rb") as wav:
            width = wav.getsampwidth()
            rate = wav.getframerate()
            total = wav.getnframes()
            window = max(1, int(rate * 0.08))
            values = []
            cursor = 0
            while cursor < total:
                frames = wav.readframes(min(window, total - cursor))
                if not frames:
                    break
                values.append(pcm_rms(frames, width))
                cursor += window
            peak = max(values) if values else 1.0
            return [min(1.0, value / max(1.0, peak * 0.72)) for value in values]
    except Exception:
        return []

def rhubarb_cues(path):
    if not path:
        return []
    try:
        payload = json.loads(Path(path).read_text(encoding="utf-8"))
        return payload.get("mouthCues", [])
    except Exception:
        return []

def find_mouth_shape(root):
    wanted = {"jawopen", "mouthopen", "visemeaa", "visemea", "aah", "open"}
    for obj in descendants(root):
        if obj.type != "MESH" or not getattr(obj.data, "shape_keys", None):
            continue
        for key in obj.data.shape_keys.key_blocks:
            if normalized(key.name) in wanted:
                return key
    return None

def fallback_mouth(root):
    for obj in descendants(root):
        if obj.get("repotunnel_mouth"):
            return obj
    return None

def animate_lipsync(actor, root, armature):
    if not actor.get("lipSync"):
        return
    audio = actor.get("dialogueAudio")
    if not audio or not audio.get("path"):
        return
    cue_path = actor.get("lipSyncCuePath")
    cues = rhubarb_cues(cue_path)
    key = find_mouth_shape(root)
    jaw = find_pose_bone(armature, "jaw") if armature else None
    proxy = fallback_mouth(root)
    if actor.get("assetPath") and not (key or jaw):
        raise RuntimeError(
            f"Character '{actor['id']}' requests lip-sync but its rig has no supported mouth shape key or jaw bone."
        )

    def set_open(frame, amount):
        frame = max(scene.frame_start, min(scene.frame_end, int(frame)))
        amount = max(0.0, min(1.0, float(amount)))
        if key:
            key.value = amount
            key.keyframe_insert(data_path="value", frame=frame)
        elif jaw:
            jaw.rotation_mode = "XYZ"
            jaw.rotation_euler.x = math.radians(13.0 * amount)
            jaw.keyframe_insert(data_path="rotation_euler", frame=frame)
        elif proxy:
            proxy.scale.z = 0.45 + amount * 2.0
            proxy.keyframe_insert(data_path="scale", frame=frame)

    if cues:
        closed = {"X", "A", "B", "M", "P"}
        for cue in cues:
            start = float(cue.get("start", 0.0))
            end = float(cue.get("end", start + 0.08))
            value = str(cue.get("value", "X")).upper()
            amount = 0.08 if value in closed else 0.78
            set_open(1 + start * scene.render.fps, amount)
            set_open(1 + end * scene.render.fps, 0.05)
        return

    amplitudes = mouth_amplitudes(audio["path"], float(data["duration"]))
    if not amplitudes:
        raise RuntimeError(
            f"Could not derive lip-sync motion from dialogue audio for character '{actor['id']}'."
        )
    for index, amount in enumerate(amplitudes):
        frame = 1 + int(round(index * 0.08 * scene.render.fps))
        set_open(frame, amount)
    set_open(scene.frame_end, 0.0)

for actor in actors:
    root = actor_roots[actor["id"]]
    armature = actor_armatures[actor["id"]]
    keyframe_root_motion(actor, root, armature)
    setup_foot_ik(actor, root, armature)
for actor in actors:
    root = actor_roots[actor["id"]]
    armature = actor_armatures[actor["id"]]
    setup_interaction(actor, root, armature)
    animate_lipsync(actor, root, armature)

def focus_point():
    points = []
    for root in actor_roots.values():
        points.append(root.matrix_world.translation + Vector((0, 0, 1.6)))
    if not points:
        for root in prop_roots.values():
            points.append(root.matrix_world.translation + Vector((0, 0, 0.6)))
    if not points:
        return Vector((0, 0, 1.4))
    total = Vector((0, 0, 0))
    for point in points:
        total += point
    return total / len(points)

def aim(obj, target):
    direction = Vector(target) - obj.location
    if direction.length > 0.0001:
        obj.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()

camera_cfg = data.get("camera") or {}
shot_type = str(camera_cfg.get("shotType") or "medium").lower()
movement = str(camera_cfg.get("movement") or "static").lower()
angle = str(camera_cfg.get("angle") or "eye-level").lower()
target = focus_point()
distance_by_shot = {
    "extreme-wide": 16.0,
    "wide": 12.5,
    "full": 10.5,
    "medium-wide": 9.5,
    "medium": 8.0,
    "medium-close": 6.6,
    "close": 5.0,
    "close-up": 4.5,
    "extreme-close": 3.5,
    "insert": 3.8,
}
distance = distance_by_shot.get(shot_type, 8.0)
height = target.z
if "high" in angle or "bird" in angle:
    height += 4.0
elif "low" in angle:
    height -= 1.2
else:
    height += 1.0

camera_data = bpy.data.cameras.new("Camera")
camera = bpy.data.objects.new("Camera", camera_data)
scene.collection.objects.link(camera)
scene.camera = camera
camera.location = (target.x, target.y - distance, height)
camera_data.lens = 52 if shot_type in ("close", "close-up", "extreme-close") else 45
aim(camera, target)
camera.keyframe_insert(data_path="location", frame=scene.frame_start)
camera.keyframe_insert(data_path="rotation_euler", frame=scene.frame_start)

end_target = target.copy()
if actors:
    primary = actor_roots[actors[0]["id"]]
    original_frame = scene.frame_current
    scene.frame_set(scene.frame_end)
    end_target = primary.matrix_world.translation.copy() + Vector((0, 0, 1.6))
    scene.frame_set(original_frame)

if movement in ("tracking", "pan"):
    camera.location.x += end_target.x - target.x
elif movement in ("dolly", "push-in"):
    camera.location.y += distance * 0.28
elif movement in ("pull-out", "zoom-out"):
    camera.location.y -= distance * 0.30
elif movement == "orbit":
    rel = camera.location - target
    angle_r = math.radians(28)
    camera.location = target + Vector((
        rel.x * math.cos(angle_r) - rel.y * math.sin(angle_r),
        rel.x * math.sin(angle_r) + rel.y * math.cos(angle_r),
        rel.z,
    ))
elif movement == "crane":
    camera.location.z += 3.2
elif movement == "handheld":
    mid = max(2, scene.frame_end // 2)
    camera.location.x += 0.16
    camera.location.z += 0.12
    aim(camera, target)
    camera.keyframe_insert(data_path="location", frame=mid)
    camera.keyframe_insert(data_path="rotation_euler", frame=mid)
    camera.location.x -= 0.26
    camera.location.z -= 0.18

aim(camera, end_target)
camera.keyframe_insert(data_path="location", frame=scene.frame_end)
camera.keyframe_insert(data_path="rotation_euler", frame=scene.frame_end)

world = bpy.data.worlds.new("StoryWorld")
scene.world = world
world.color = (0.035, 0.045, 0.07)

bpy.ops.object.light_add(type="AREA", location=(0, -3, 8))
key_light = bpy.context.object
key_light.data.energy = 1200
key_light.data.shape = "DISK"
key_light.data.size = 8
bpy.ops.object.light_add(type="AREA", location=(4, 2, 5))
fill = bpy.context.object
fill.data.energy = 650
fill.data.size = 6

scene.render.film_transparent = False
bpy.ops.render.render(animation=True)
