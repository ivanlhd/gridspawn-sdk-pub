extends Node
## Headless test of the game server's voice rules, driven by
## sdk/godot/gridspawn/tests/headless.rs (headless_voice_rules): two instances
## in a session whose game server configured the room `match` with channels.
## Both start in the same channel. Reads GRIDSPAWN_TEST_APP_ID,
## GRIDSPAWN_TEST_TICKET, GRIDSPAWN_TEST_TONE_HZ (own tone),
## GRIDSPAWN_TEST_EXPECT_HZ (the other's) and GRIDSPAWN_TEST_SPATIAL
## ("server" or "client"); prints RESULT and INFO lines.
##
## Phases (the harness moves the other player between channels when both
## instances have printed the phase's end):
## 1. same channel: the other is heard → "RESULT phase1_done";
## 2. other channel: audibility_changed(audible = false), silence →
##    "RESULT phase2_done";
## 3. same channel again: audibility_changed(audible = true), heard again.
##
## A third player (GRIDSPAWN_TEST_MUTED_NAME), muted by the game server before
## both instances join, must be reported muted at join:
## server_mute_changed(id, true) and server_muted in get_participants().

const RATE := 48000.0
const LISTEN_SECONDS := 3.0

var _voice: GridspawnVoice
var _spatial := "server"
var _tone_hz := 0.0
var _expect_hz := 0.0
var _phase := 0.0
var _sent := 0
## Tone pacing by system-time deltas (see voice_test.gd for why).
var _elapsed := 0.0
var _last_wall := 0.0
var _last_ticks := 0
var _sending := false
var _other := ""
var _capture: AudioEffectCapture
var _heard := PackedVector2Array()
var _listening := false
var _step := "attach"
var _room_reported := false
var _anchor: Node3D
var _muted_name := ""
var _muted_id := ""
var _muted_reported := false


func _ready() -> void:
	_spatial = OS.get_environment("GRIDSPAWN_TEST_SPATIAL")
	if _spatial != "client":
		_spatial = "server"
	_tone_hz = float(OS.get_environment("GRIDSPAWN_TEST_TONE_HZ"))
	_expect_hz = float(OS.get_environment("GRIDSPAWN_TEST_EXPECT_HZ"))
	_muted_name = OS.get_environment("GRIDSPAWN_TEST_MUTED_NAME")
	Gridspawn.app_id = OS.get_environment("GRIDSPAWN_TEST_APP_ID")
	Gridspawn.environment = 0  # Development
	Gridspawn.token_storage = 1  # Memory
	Gridspawn.attached.connect(_on_attached)
	Gridspawn.attach_failed.connect(func(code, _message): _fail("attach_failed " + code))

	_voice = GridspawnVoice.new()
	_voice.input_source = 1  # Custom
	_voice.echo_cancellation = false
	_voice.noise_suppression = false
	_voice.auto_gain = false
	add_child(_voice)
	_voice.joined.connect(_on_joined)
	_voice.participant_joined.connect(_on_participant_joined)
	_voice.room_config_changed.connect(_on_room_config)
	_voice.audibility_changed.connect(_on_audibility)
	_voice.server_mute_changed.connect(_on_server_mute)
	_voice.left.connect(_on_left)
	_voice.error.connect(_on_error)

	_capture = AudioEffectCapture.new()
	_capture.buffer_length = 1.0
	AudioServer.add_bus_effect(0, _capture)

	get_tree().create_timer(150.0).timeout.connect(func(): _fail("timeout in step " + _step))
	Gridspawn.attach(OS.get_environment("GRIDSPAWN_TEST_TICKET"))


func _on_attached(_session_id: String, _display_name: String, _capabilities: PackedStringArray) -> void:
	_expect("attach")
	_step = "join"
	_voice.join()


func _on_joined(room: String) -> void:
	_expect("join")
	print("RESULT joined " + room)
	var steps := []
	for step in _voice.get_stats()["join_ms"]:
		steps.append("%s %d" % [step[0], step[1]])
	print("INFO join_ms " + ", ".join(steps))
	_step = "wait_other"
	_sending = true
	_elapsed = 0.0
	_sent = 0
	if _voice.get_room_config().get("managed", false):
		_on_room_config(_voice.get_room_config())


func _on_room_config(config: Dictionary) -> void:
	if _room_reported:
		return
	_room_reported = true
	print("RESULT room managed=%s mode=%s spatial=%s secrecy_safe=%s is_secrecy_safe=%s" % [
		config.get("managed"), config.get("mode"), config.get("spatial"),
		config.get("secrecy_safe"), _voice.is_secrecy_safe()])
	_maybe_bind()


func _on_participant_joined(participant_id: String, display_name: String) -> void:
	if _muted_name != "" and display_name == _muted_name:
		_muted_id = participant_id
		return
	if _other != "" or _step != "wait_other":
		return
	_other = participant_id
	print("RESULT participant_joined display_name=" + display_name)
	_maybe_bind()
	_step = "phase1"
	get_tree().create_timer(1.0).timeout.connect(_start_listening)


## Client-spatialized rooms: place the other player 2 m in front of a camera.
## Server-spatialized rooms: binding is refused (the error is checked).
func _maybe_bind() -> void:
	if _other == "" or not _room_reported:
		return
	if _spatial == "client":
		if _anchor == null:
			var camera := Camera3D.new()
			add_child(camera)
			camera.make_current()
			_anchor = Node3D.new()
			add_child(_anchor)
			_anchor.position = Vector3(0, 0, -2)
			_voice.bind_participant(_other, _anchor)
			print("RESULT bound")
	elif _anchor == null:
		_anchor = Node3D.new()
		add_child(_anchor)
		_voice.bind_participant(_other, _anchor)


func _on_error(code: String, message: String) -> void:
	if code == "voice/not_client_spatial" and _spatial == "server":
		print("RESULT bind_refused " + code)
		return
	_fail("voice error %s %s" % [code, message])


## The muted player is reported muted right after joining (it was muted
## before this instance joined, so no later change announces it).
func _on_server_mute(participant_id: String, muted: bool) -> void:
	if participant_id != _muted_id or _muted_reported:
		return
	_muted_reported = true
	var listed := false
	for participant in _voice.get_participants():
		if participant["participant_id"] == _muted_id:
			listed = participant["server_muted"]
	print("INFO muted player reported in step %s" % _step)
	print("RESULT server_muted_at_join muted=%s listed=%s" % [muted, listed])


func _on_audibility(participant_id: String, audible: bool, gain: float, pan: Variant) -> void:
	if participant_id != _other:
		return
	print("INFO audibility audible=%s gain=%.4f pan=%s in step %s" % [audible, gain, pan, _step])
	if _step == "wait_inaudible" and not audible:
		print("RESULT audibility audible=false")
		_step = "phase2"
		get_tree().create_timer(1.0).timeout.connect(_start_listening)
	elif _step == "wait_audible" and audible:
		print("RESULT audibility audible=true")
		_step = "phase3"
		get_tree().create_timer(1.0).timeout.connect(_start_listening)


func _start_listening() -> void:
	_heard = PackedVector2Array()
	_listening = true
	get_tree().create_timer(LISTEN_SECONDS).timeout.connect(_report)


func _process(_delta: float) -> void:
	var now := Time.get_ticks_usec()
	var frames := _capture.get_frames_available()
	if frames > 0:
		var buffer := _capture.get_buffer(frames)
		if _listening:
			_heard.append_array(buffer)
	var wall := Time.get_unix_time_from_system()
	if _last_wall > 0.0:
		var step := wall - _last_wall
		if step < 0.0 or step > 0.25:
			step = (now - _last_ticks) / 1000000.0
		_elapsed += step
	_last_wall = wall
	_last_ticks = now
	if _sending:
		var due := int(_elapsed * RATE)
		var count := due - _sent
		if count > 0:
			var samples := PackedFloat32Array()
			samples.resize(count)
			for i in count:
				_phase += TAU * _tone_hz / RATE
				samples[i] = 0.3 * sin(_phase)
			_voice.push_input(samples)
			_sent = due


func _report() -> void:
	_listening = false
	var rate := AudioServer.get_mix_rate()
	var expected := _power(_heard, _expect_hz, rate)
	var own := _power(_heard, _tone_hz, rate)
	var off := _power(_heard, 1000.0, rate)
	var hears: bool = expected > 100.0 * max(own, off) and expected > 1e-6
	print("INFO %s: expected tone power %.6f (own %.6f, 1 kHz %.6f), %d frames" % [
		_step, expected, own, off, _heard.size()])
	print("RESULT hears_other_tone=%s" % hears)
	if _step == "phase1":
		_step = "wait_inaudible"
		print("RESULT phase1_done")
	elif _step == "phase2":
		_step = "wait_audible"
		print("RESULT phase2_done")
	elif _step == "phase3":
		_step = "leave"
		_voice.leave()


func _on_left(reason: String) -> void:
	_expect("leave")
	print("RESULT left " + reason)
	print("RESULT done")
	get_tree().quit(0)


## Tone power, Hann-windowed 20 ms frames summed (robust to codec drift).
func _power(frames: PackedVector2Array, hz: float, rate: float) -> float:
	var size := int(rate * 0.02)
	var k := 2.0 * cos(TAU * hz / rate)
	var total := 0.0
	var start := 0
	while start + size <= frames.size():
		var s1 := 0.0
		var s2 := 0.0
		for n in size:
			var w := 0.5 - 0.5 * cos(TAU * n / (size - 1))
			var x := w * 0.5 * (frames[start + n].x + frames[start + n].y)
			var s0 := x + k * s1 - s2
			s2 = s1
			s1 = s0
		total += (s1 * s1 + s2 * s2 - k * s1 * s2) / size
		start += size
	return total


func _expect(step: String) -> void:
	if _step != step:
		_fail("signal in step %s, expected %s" % [_step, step])


func _fail(reason: String) -> void:
	print("RESULT failed " + reason)
	get_tree().quit(1)
