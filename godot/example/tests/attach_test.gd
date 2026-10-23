extends Node
## Headless test, driven by sdk/godot/gridspawn/tests/headless.rs.
## Reads GRIDSPAWN_TEST_APP_ID and GRIDSPAWN_TEST_TICKET; prints RESULT lines.

const FORGED_TICKET := "gpt1.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

var _step := "restore"


func _ready() -> void:
	Gridspawn.app_id = OS.get_environment("GRIDSPAWN_TEST_APP_ID")
	Gridspawn.environment = 0  # Development
	Gridspawn.token_storage = 1  # Memory
	Gridspawn.signed_in.connect(func(_name): _fail("signed_in"))
	Gridspawn.sign_in_failed.connect(func(code, _message): _fail("sign_in_failed " + code))
	Gridspawn.signed_out.connect(_on_signed_out)
	Gridspawn.assertion_ready.connect(func(_assertion): _fail("assertion_ready"))
	Gridspawn.assertion_failed.connect(_on_assertion_failed)
	Gridspawn.attached.connect(_on_attached)
	Gridspawn.attach_failed.connect(_on_attach_failed)
	Gridspawn.presence_set.connect(func(_expires_at): _fail("presence_set"))
	Gridspawn.presence_failed.connect(_on_presence_failed)
	Gridspawn.grant_renewed.connect(_on_grant_renewed)
	Gridspawn.grant_expired.connect(func(module, code): _fail("grant_expired %s %s" % [module, code]))
	get_tree().create_timer(90.0).timeout.connect(func(): _fail("timeout in step " + _step))
	Gridspawn.restore()


func _on_signed_out() -> void:
	_expect("restore")
	print("RESULT signed_out signed_in=%s persist=%s" % [Gridspawn.is_signed_in(), Gridspawn.sessions_persist()])
	print("RESULT sign_in_method='%s' browser_only=%s" % [Gridspawn.sign_in_method(), Gridspawn.browser_only])
	_step = "assertion"
	Gridspawn.player_assertion("headless-check-challenge-0123456789-abcdef")


func _on_assertion_failed(code: String, _message: String) -> void:
	_expect("assertion")
	print("RESULT assertion_failed " + code)
	_step = "attach"
	Gridspawn.attach(OS.get_environment("GRIDSPAWN_TEST_TICKET"))


func _on_attached(session_id: String, display_name: String, capabilities: PackedStringArray) -> void:
	_expect("attach")
	print("RESULT attached session=%s name=%s capabilities=%s" % [not session_id.is_empty(), display_name, ",".join(capabilities)])
	_step = "renew"
	Gridspawn.renew()


func _on_grant_renewed(module: String) -> void:
	_expect("renew")
	print("RESULT grant_renewed " + module)
	_step = "forged"
	Gridspawn.detach()
	Gridspawn.attach(FORGED_TICKET)


func _on_attach_failed(code: String, _message: String) -> void:
	_expect("forged")
	print("RESULT attach_failed " + code)
	# Presence: no session grant for it (guests get none) and no signed-in
	# player, so it is refused before anything is sent.
	_step = "presence"
	Gridspawn.set_presence("Live check", "", "")


func _on_presence_failed(code: String, _message: String) -> void:
	_expect("presence")
	print("RESULT presence_failed " + code)
	print("RESULT done")
	get_tree().quit(0)


func _expect(step: String) -> void:
	if _step != step:
		_fail("signal in step %s, expected %s" % [_step, step])


func _fail(reason: String) -> void:
	print("RESULT failed " + reason)
	get_tree().quit(1)
