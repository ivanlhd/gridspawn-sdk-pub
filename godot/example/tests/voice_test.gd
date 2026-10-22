extends Node
## Headless voice test, driven by sdk/godot/gridspawn/tests/headless.rs: two
## instances join one session's voice room, each sending its own tone through
## a custom input. Reads GRIDSPAWN_TEST_APP_ID, GRIDSPAWN_TEST_TICKET,
## GRIDSPAWN_TEST_TONE_HZ (own tone) and GRIDSPAWN_TEST_EXPECT_HZ (the other
## instance's tone); prints RESULT and INFO lines.

const RATE := 48000.0
const LISTEN_SECONDS := 4.0
const STAY_SECONDS := 3.0

var _voice: GridspawnVoice
var _tone_hz := 0.0
var _expect_hz := 0.0
var _phase := 0.0
var _sent := 0
## Elapsed time (s) that paces the tone: the system time, minus its steps. Under
## WSL2, Time.get_ticks_* (CLOCK_MONOTONIC_RAW) ran about 11 % fast against the
## monotonic clock the voice core times packets with, and the system time
## followed that monotonic clock but stepped about +3.6 s every 32 s when the VM
## re-synced with the host. The audio mix clock had the right rate but came in
## large chunks. So: system-time deltas, and a frame's tick delta instead when
## the system time jumps.
var _elapsed := 0.0
var _last_wall := 0.0
var _last_ticks := 0
var _sending := false
var _other := ""
var _capture: AudioEffectCapture
var _heard := PackedVector2Array()
var _listening := false
var _step := "attach"
var _window_start := 0.0
var _window_sent := 0
var _window_received := 0
var _last_frame_usec := 0
var _frames := 0
var _longest_frame_usec := 0


func _ready() -> void:
	_tone_hz = float(OS.get_environment("GRIDSPAWN_TEST_TONE_HZ"))
	_expect_hz = float(OS.get_environment("GRIDSPAWN_TEST_EXPECT_HZ"))
	Gridspawn.app_id = OS.get_environment("GRIDSPAWN_TEST_APP_ID")
	Gridspawn.environment = 0  # Development
	Gridspawn.token_storage = 1  # Memory
	Gridspawn.attached.connect(_on_attached)
	Gridspawn.attach_failed.connect(func(code, _message): _fail("attach_failed " + code))

	_voice = GridspawnVoice.new()
	_voice.input_source = 1  # Custom: this script's tone instead of a microphone
	# Steady tones: the processing would reshape them.
	_voice.echo_cancellation = false
	_voice.noise_suppression = false
	_voice.auto_gain = false
	add_child(_voice)
	_voice.joined.connect(_on_joined)
	_voice.participant_joined.connect(_on_participant_joined)
	_voice.left.connect(_on_left)
	_voice.error.connect(func(code, message): _fail("voice error %s %s" % [code, message]))

	# What this instance plays (the other's voice), read back from Master.
	_capture = AudioEffectCapture.new()
	_capture.buffer_length = 1.0
	AudioServer.add_bus_effect(0, _capture)

	get_tree().create_timer(90.0).timeout.connect(func(): _fail("timeout in step " + _step))
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


func _on_participant_joined(participant_id: String, display_name: String) -> void:
	if _other != "":
		return
	_other = participant_id
	print("RESULT participant_joined display_name_empty=%s" % display_name.is_empty())
	_step = "listen"
	get_tree().create_timer(1.0).timeout.connect(_start_listening)


func _start_listening() -> void:
	_heard = PackedVector2Array()
	_listening = true
	_window_start = _elapsed
	var stats: Dictionary = _voice.get_stats()
	_window_sent = stats["packets_sent"]
	_window_received = stats["participants"].get(_other, {}).get("packets_received", 0)
	get_tree().create_timer(LISTEN_SECONDS).timeout.connect(_report)


func _process(_delta: float) -> void:
	var now := Time.get_ticks_usec()
	if _listening and _last_frame_usec > 0:
		_frames += 1
		_longest_frame_usec = max(_longest_frame_usec, now - _last_frame_usec)
	_last_frame_usec = now
	# What this instance played since the last frame (read always, kept while listening).
	var frames := _capture.get_frames_available()
	if frames > 0:
		var buffer := _capture.get_buffer(frames)
		if _listening:
			_heard.append_array(buffer)
	var wall := Time.get_unix_time_from_system()
	if _last_wall > 0.0:
		var step := wall - _last_wall
		if step < 0.0 or step > 0.25:
			step = (now - _last_ticks) / 1000000.0  # the system time jumped
		_elapsed += step
	_last_wall = wall
	_last_ticks = now
	if _sending:
		# As many samples as the elapsed time asks for.
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
	var stats: Dictionary = _voice.get_stats()
	var mine: Dictionary = stats["participants"].get(_other, {})
	print("INFO stats sent=%d received=%d from_data=%d concealed=%d target_delay_ms=%d played_frames=%d mix_rate=%d" % [
		stats["packets_sent"], mine.get("packets_received", 0), mine.get("frames_from_data", 0),
		mine.get("frames_concealed", 0), mine.get("target_delay_ms", 0), _heard.size(),
		AudioServer.get_mix_rate()])
	var window := _elapsed - _window_start
	print("INFO packets/s over the listening window (%.2f s): sent %.1f, received %.1f" % [
		window, (stats["packets_sent"] - _window_sent) / window,
		(mine.get("packets_received", 0) - _window_received) / window])
	print("INFO frames while listening: %d in %.0f s, longest %.1f ms" % [_frames, LISTEN_SECONDS, _longest_frame_usec / 1000.0])
	print("RESULT network sent=%s received=%s" % [stats["packets_sent"] > 100, mine.get("packets_received", 0) > 100])
	if _heard.size() > 0:
		var rate := AudioServer.get_mix_rate()
		var expected := _power(_heard, _expect_hz, rate)
		var own := _power(_heard, _tone_hz, rate)
		var off := _power(_heard, 1000.0, rate)
		print("INFO tone power: expected %.1f dB above own tone, %.1f dB above 1 kHz" % [_db(expected / max(own, 1e-12)), _db(expected / max(off, 1e-12))])
		print("RESULT hears_other_tone=%s" % (expected > 100.0 * max(own, off) and expected > 1e-6))
	else:
		print("RESULT hears_other_tone=no_audio_mixed")
	_step = "leave"
	get_tree().create_timer(STAY_SECONDS).timeout.connect(func(): _voice.leave())


func _on_left(reason: String) -> void:
	_expect("leave")
	print("RESULT left " + reason)
	print("RESULT done")
	get_tree().quit(0)


func _db(ratio: float) -> float:
	return 10.0 * log(max(ratio, 1e-12)) / log(10.0)


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
