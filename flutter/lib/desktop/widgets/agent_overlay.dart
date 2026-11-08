import 'dart:async';
import 'dart:math' as math;
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common.dart' hide Dialog;
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/models/agent_bridge.dart';
import 'package:flutter_hbb/models/agent_model.dart';
import 'package:flutter_hbb/models/agent_note.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/platform_model.dart';

const _agentGreen = Color(0xFF22C55E);
const _blockRed = Color(0xFFDC2626);

/// One `agent_state` event from the session: mirror it, copy a fresh share link, and show the PC's words.
void onAgentStateEvent(SessionID sessionId, String stateJson) {
  final update = AgentModel.of(sessionId).apply(stateJson);
  final link = update.link;
  if (link != null) {
    Clipboard.setData(ClipboardData(text: link));
    showToast('Share link copied. Paste it to a Rig agent; it works once.');
  }
  if (update.note.isNotEmpty) showToast(update.note);
}

/// The agent host each open session's panel talks to, once set up for that PC.
final Map<String, AgentBridge> _bridges = {};

String _hostOptionKey(FFI ffi) => 'rigdesk-agent-host-${ffi.id}';

Future<void> _ask(FFI ffi, String verb,
    {String text = '', Uint8List? png, int display = 0}) async {
  final bridge = _bridges['${ffi.sessionId}'];
  if (bridge == null) {
    showToast('Set up the agent host for this PC first.');
    return;
  }
  await bridge.ask(verb, text: text, png: png, display: display);
}

/// The agent controls over the remote image: Share with agent, the owner badge, Hand to agent / Take back, Stop,
/// Block agent, the message box, plus the agent's labelled cursor and its action strip. The agent itself runs in
/// Rig; its session to this PC is the agent host's (rigdesk-remote on the Rig host), which this panel drives.
class AgentOverlay extends StatefulWidget {
  final FFI ffi;

  const AgentOverlay({Key? key, required this.ffi}) : super(key: key);

  @override
  State<AgentOverlay> createState() => _AgentOverlayState();
}

class _AgentOverlayState extends State<AgentOverlay> {
  FFI get ffi => widget.ffi;

  @override
  void initState() {
    super.initState();
    HardwareKeyboard.instance.addHandler(_onKey);
    final saved = AgentHost.fromJson(bind.mainGetLocalOption(key: _hostOptionKey(ffi)));
    if (saved != null) _connect(saved);
  }

  @override
  void dispose() {
    HardwareKeyboard.instance.removeHandler(_onKey);
    _bridges.remove('${ffi.sessionId}')?.close();
    super.dispose();
  }

  void _connect(AgentHost host) {
    _bridges.remove('${ffi.sessionId}')?.close();
    final bridge = AgentBridge(host,
        onState: (state) => onAgentStateEvent(ffi.sessionId, state),
        onTrouble: showToast);
    _bridges['${ffi.sessionId}'] = bridge;
    bridge.start();
    if (mounted) setState(() {});
  }

  bool get _hasHost => _bridges.containsKey('${ffi.sessionId}');

  /// Ctrl+Alt+Esc stops the agent and never reaches the PC. Any other key while the agent holds input takes it back.
  bool _onKey(KeyEvent event) {
    if (event is! KeyDownEvent) return false;
    final agent = AgentModel.of(ffi.sessionId);
    final keys = HardwareKeyboard.instance;
    if (event.logicalKey == LogicalKeyboardKey.escape &&
        keys.isControlPressed &&
        keys.isAltPressed &&
        agent.isShared) {
      _ask(ffi, 'stop');
      return true;
    }
    final onTop = mounted && (ModalRoute.of(context)?.isCurrent ?? true);
    if (agent.agentHolds && onTop) _ask(ffi, 'take_back');
    return false;
  }

  Future<void> _setUp(BuildContext context) async {
    final saved = AgentHost.fromJson(bind.mainGetLocalOption(key: _hostOptionKey(ffi)));
    final host = await showDialog<AgentHost>(
        context: context, builder: (_) => AgentHostDialog(initial: saved, peer: ffi.id));
    if (host == null) return;
    await bind.mainSetLocalOption(key: _hostOptionKey(ffi), value: host.toJson());
    _connect(host);
  }

  @override
  Widget build(BuildContext context) {
    final agent = AgentModel.of(ffi.sessionId);
    return AnimatedBuilder(
      animation: Listenable.merge([agent, ffi.canvasModel]),
      builder: (context, _) {
        final children = <Widget>[
          Positioned.fill(
            child: Listener(
              behavior: HitTestBehavior.translucent,
              onPointerDown: (_) {
                if (agent.agentHolds) _ask(ffi, 'take_back');
              },
            ),
          ),
        ];
        final cursor = _cursor(agent);
        if (cursor != null) children.add(cursor);
        if (agent.isShared && agent.actions.isNotEmpty) {
          children.add(Positioned(
              left: 10, bottom: 10, child: IgnorePointer(child: _strip(agent))));
        }
        children.add(
            Positioned(right: 10, bottom: 10, child: _bar(context, agent)));
        return Stack(children: children);
      },
    );
  }

  Widget? _cursor(AgentModel agent) {
    final point = agent.pointer;
    final rect = ffi.ffiModel.rect;
    if (!agent.isShared || point == null || rect == null) return null;
    final c = ffi.canvasModel;
    final left = (point.x!.toDouble() - rect.left) * c.scale + c.x;
    final top = (point.y!.toDouble() - rect.top) * c.scale + c.y;
    return Positioned(
      left: left - 7,
      top: top - 7,
      child: IgnorePointer(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Container(
              width: 14,
              height: 14,
              decoration: BoxDecoration(
                color: _agentGreen,
                shape: BoxShape.circle,
                border: Border.all(color: Colors.white, width: 2),
              ),
            ),
            const SizedBox(width: 4),
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
              decoration: BoxDecoration(
                color: _agentGreen,
                borderRadius: BorderRadius.circular(4),
              ),
              child: Text(
                '${point.agentTitle}: ${point.summary}',
                style: const TextStyle(color: Colors.white, fontSize: 12),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _strip(AgentModel agent) {
    return Container(
      constraints: const BoxConstraints(maxWidth: 380),
      padding: const EdgeInsets.all(8),
      decoration: BoxDecoration(
        color: Colors.black.withOpacity(0.6),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: agent.actions
            .map((a) => Text(
                  '${a.ok ? '✓' : '✗'} ${a.agentTitle}: ${a.summary}',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                      color: a.ok ? Colors.white : Colors.orangeAccent,
                      fontSize: 12),
                ))
            .toList(),
      ),
    );
  }

  Widget _bar(BuildContext context, AgentModel agent) {
    if (!_hasHost) {
      return Material(
        elevation: 4,
        borderRadius: BorderRadius.circular(8),
        child: TextButton.icon(
          onPressed: () => _setUp(context),
          icon: const Icon(Icons.smart_toy, size: 18),
          label: const Text('Set up agent host'),
        ),
      );
    }
    final items = <Widget>[
      Tooltip(
        message: 'Copy a one-time link that hands this session to a Rig agent',
        child: TextButton.icon(
          onPressed: () => _ask(ffi, 'share'),
          icon: const Icon(Icons.share, size: 18),
          label: Text(agent.linkOpen && !agent.isShared
              ? 'Link copied, waiting for the agent'
              : 'Share with agent'),
        ),
      ),
    ];
    if (agent.isShared) {
      final holds = agent.isBlocked
          ? 'blocked'
          : agent.agentHolds
              ? 'has input'
              : 'waiting';
      items.add(Tooltip(
        message: 'Rig agent ${agent.ownerTitle} in ${agent.ownerGraph}',
        child: Chip(
          avatar: const Icon(Icons.smart_toy, size: 18, color: _agentGreen),
          label: Text('${agent.ownerTitle} · $holds'),
        ),
      ));
      if (agent.holder == 'human') {
        items.add(TextButton(
            onPressed: () => _ask(ffi, 'hand'),
            child: const Text('Hand to agent')));
      } else if (agent.agentHolds) {
        items.add(TextButton(
            onPressed: () => _ask(ffi, 'take_back'),
            child: const Text('Take back')));
      }
      items.add(Tooltip(
        message: 'Message the agent, with a screenshot',
        child: IconButton(
          onPressed: () => showDialog(
            context: context,
            barrierDismissible: false,
            builder: (_) => AgentNoteDialog(ffi: ffi),
          ),
          icon: const Icon(Icons.chat_bubble_outline),
        ),
      ));
      items.add(Tooltip(
        message: 'End the share',
        child: IconButton(
          onPressed: () => _ask(ffi, 'revoke'),
          icon: const Icon(Icons.link_off),
        ),
      ));
      items.add(TextButton(
        onPressed: () => _ask(ffi, agent.isBlocked ? 'unblock' : 'block'),
        child: Text(agent.isBlocked ? 'Unblock' : 'Block'),
      ));
      items.add(const SizedBox(width: 6));
      items.add(Tooltip(
        message: 'Take input back and stop the agent now (Ctrl+Alt+Esc)',
        child: ElevatedButton.icon(
          style: ElevatedButton.styleFrom(
            backgroundColor: _blockRed,
            foregroundColor: Colors.white,
            padding: const EdgeInsets.symmetric(horizontal: 22, vertical: 16),
            textStyle:
                const TextStyle(fontSize: 16, fontWeight: FontWeight.bold),
          ),
          onPressed: () => _ask(ffi, 'stop'),
          icon: const Icon(Icons.stop_circle),
          label: const Text('Stop'),
        ),
      ));
    }
    items.add(Tooltip(
      message: 'Agent host settings for this PC',
      child: IconButton(
        onPressed: () => _setUp(context),
        icon: const Icon(Icons.settings, size: 18),
      ),
    ));
    return Material(
      elevation: 4,
      borderRadius: BorderRadius.circular(8),
      child: Padding(
        padding: const EdgeInsets.all(6),
        child: Row(mainAxisSize: MainAxisSize.min, children: items),
      ),
    );
  }
}

/// Where this PC's agent host is: the rigdesk-remote Rig runs for it, its token, and the PC's name in Rig.
class AgentHostDialog extends StatefulWidget {
  final AgentHost? initial;
  final String peer;

  const AgentHostDialog({Key? key, this.initial, required this.peer}) : super(key: key);

  @override
  State<AgentHostDialog> createState() => _AgentHostDialogState();
}

class _AgentHostDialogState extends State<AgentHostDialog> {
  late final _url = TextEditingController(text: widget.initial?.url ?? 'http://127.0.0.1:21120');
  late final _token = TextEditingController(text: widget.initial?.token ?? '');
  late final _name = TextEditingController(text: widget.initial?.name ?? '');

  @override
  void dispose() {
    _url.dispose();
    _token.dispose();
    _name.dispose();
    super.dispose();
  }

  AgentHost get _host => AgentHost(
      url: _url.text.trim().replaceAll(RegExp(r'/+$'), ''),
      token: _token.text.trim(),
      name: _name.text.trim());

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text('Agent host for ${widget.peer}'),
      content: SizedBox(
        width: 460,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextField(
                controller: _url,
                decoration: const InputDecoration(
                    labelText: 'Agent host URL',
                    helperText: 'The rigdesk-remote Rig runs for this PC')),
            TextField(
                controller: _token,
                obscureText: true,
                decoration: const InputDecoration(labelText: 'Token')),
            TextField(
                controller: _name,
                decoration: const InputDecoration(
                    labelText: 'PC name in Rig',
                    helperText: 'As named in plugins.rigdesk.pcs')),
          ],
        ),
      ),
      actions: [
        TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Cancel')),
        ElevatedButton(
            onPressed: () {
              if (_host.isComplete) Navigator.of(context).pop(_host);
            },
            child: const Text('Save')),
      ],
    );
  }
}

/// The message box: words for the agent, with a still of the screen that can be clipped and drawn on.
class AgentNoteDialog extends StatefulWidget {
  final FFI ffi;

  const AgentNoteDialog({Key? key, required this.ffi}) : super(key: key);

  @override
  State<AgentNoteDialog> createState() => _AgentNoteDialogState();
}

class _AgentNoteDialogState extends State<AgentNoteDialog> {
  static const _areaWidth = 860.0;
  static const _areaHeight = 480.0;

  final _text = TextEditingController();
  ui.Image? _frame;
  bool _loading = true;
  String _error = '';
  NoteTool _tool = NoteTool.pen;
  final List<NoteMark> _marks = [];
  NoteMark? _drawing;
  Rect? _clip;
  Offset? _clipStart;
  bool _sending = false;

  @override
  void initState() {
    super.initState();
    _fetch();
  }

  @override
  void dispose() {
    _frame?.dispose();
    _text.dispose();
    super.dispose();
  }

  double get _viewScale {
    final frame = _frame;
    if (frame == null) return 1;
    return math.min(
        1.0,
        math.min(
            _areaWidth / frame.width, _areaHeight / frame.height));
  }

  Future<void> _fetch() async {
    final agent = AgentModel.of(widget.ffi.sessionId);
    final waiter = Completer<Uint8List?>();
    agent.frameWaiter = waiter;
    final display = widget.ffi.ffiModel.pi.currentDisplay;
    await _ask(widget.ffi, 'frame',
        display: display == kAllDisplayValue ? 0 : display);
    Uint8List? png;
    try {
      png = await waiter.future.timeout(const Duration(seconds: 15));
    } on TimeoutException {
      png = null;
    }
    ui.Image? image;
    if (png != null && png.isNotEmpty) {
      try {
        image = await decodeImageFromList(png);
      } catch (e) {
        image = null;
      }
    }
    if (!mounted) {
      image?.dispose();
      return;
    }
    setState(() {
      _frame = image;
      _loading = false;
      if (image == null) {
        _error = 'No screenshot could be taken; the message goes without one.';
      }
    });
  }

  Offset _toImage(Offset local) {
    final frame = _frame!;
    final p = local / _viewScale;
    return Offset(p.dx.clamp(0.0, frame.width.toDouble()),
        p.dy.clamp(0.0, frame.height.toDouble()));
  }

  void _start(Offset local) {
    final p = _toImage(local);
    setState(() {
      if (_tool == NoteTool.clip) {
        _clipStart = p;
        _clip = null;
      } else if (_tool == NoteTool.pen) {
        _drawing = NoteMark(NoteTool.pen, [p]);
      } else if (_tool == NoteTool.arrow || _tool == NoteTool.box) {
        _drawing = NoteMark(_tool, [p, p]);
      }
    });
  }

  void _move(Offset local) {
    final p = _toImage(local);
    setState(() {
      final start = _clipStart;
      final drawing = _drawing;
      if (_tool == NoteTool.clip && start != null) {
        _clip = Rect.fromPoints(start, p);
      } else if (drawing != null && drawing.tool == NoteTool.pen) {
        drawing.points.add(p);
      } else if (drawing != null) {
        drawing.points[1] = p;
      }
    });
  }

  void _end() {
    setState(() {
      final clip = _clip;
      if (_tool == NoteTool.clip && clip != null) {
        if (clip.width < 8 || clip.height < 8) _clip = null;
      }
      _clipStart = null;
      final drawing = _drawing;
      if (drawing != null) _marks.add(drawing);
      _drawing = null;
    });
  }

  Future<void> _addText(Offset local) async {
    final at = _toImage(local);
    final controller = TextEditingController();
    final text = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Label'),
        content: TextField(
          controller: controller,
          autofocus: true,
          onSubmitted: (v) => Navigator.of(ctx).pop(v),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('Cancel')),
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(controller.text),
              child: const Text('Add')),
        ],
      ),
    );
    controller.dispose();
    if (text == null || text.trim().isEmpty || !mounted) return;
    setState(() => _marks.add(NoteMark(NoteTool.text, [at], text: text.trim())));
  }

  Future<void> _send() async {
    final words = _text.text.trim();
    if (words.isEmpty || _sending) return;
    setState(() => _sending = true);
    final frame = _frame;
    final png = frame == null
        ? Uint8List(0)
        : await bakeNote(frame, _clip, _marks, strokeScale: 1 / _viewScale);
    await _ask(widget.ffi, 'note', text: words, png: png);
    if (mounted) Navigator.of(context).pop();
  }

  Widget _tools() {
    const labels = {
      NoteTool.clip: 'Clip',
      NoteTool.pen: 'Pen',
      NoteTool.arrow: 'Arrow',
      NoteTool.box: 'Box',
      NoteTool.text: 'Text',
    };
    return Wrap(
      spacing: 6,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        for (final entry in labels.entries)
          ChoiceChip(
            label: Text(entry.value),
            selected: _tool == entry.key,
            onSelected: (_) => setState(() => _tool = entry.key),
          ),
        TextButton(
          onPressed: _marks.isEmpty
              ? null
              : () => setState(() => _marks.removeLast()),
          child: const Text('Undo'),
        ),
        TextButton(
          onPressed: _clip == null ? null : () => setState(() => _clip = null),
          child: const Text('Whole screen'),
        ),
      ],
    );
  }

  Widget _canvas() {
    final frame = _frame;
    if (_loading) {
      return const SizedBox(
          width: _areaWidth,
          height: _areaHeight,
          child: Center(child: CircularProgressIndicator()));
    }
    if (frame == null) {
      return SizedBox(
          width: _areaWidth,
          height: 60,
          child: Center(child: Text(_error)));
    }
    final s = _viewScale;
    final drawing = _drawing;
    return SizedBox(
      width: frame.width * s,
      height: frame.height * s,
      child: GestureDetector(
        onPanStart: (d) => _start(d.localPosition),
        onPanUpdate: (d) => _move(d.localPosition),
        onPanEnd: (_) => _end(),
        onTapUp: _tool == NoteTool.text
            ? (d) => _addText(d.localPosition)
            : null,
        child: CustomPaint(
          painter: _NotePainter(
            frame: frame,
            marks: [..._marks, if (drawing != null) drawing],
            clip: _clip,
            scale: s,
          ),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Dialog(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Message ${AgentModel.of(widget.ffi.sessionId).ownerTitle}',
                style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            if (_frame != null) _tools(),
            const SizedBox(height: 8),
            _canvas(),
            const SizedBox(height: 8),
            SizedBox(
              width: _areaWidth,
              child: TextField(
                controller: _text,
                autofocus: true,
                minLines: 1,
                maxLines: 4,
                decoration: const InputDecoration(
                    hintText: 'What should the agent know or do?'),
              ),
            ),
            const SizedBox(height: 8),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                TextButton(
                    onPressed: () => Navigator.of(context).pop(),
                    child: const Text('Cancel')),
                const SizedBox(width: 8),
                ElevatedButton(
                    onPressed: _sending ? null : _send,
                    child: Text(_clip == null ? 'Send with screenshot' : 'Send with clip')),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

class _NotePainter extends CustomPainter {
  final ui.Image frame;
  final List<NoteMark> marks;
  final Rect? clip;
  final double scale;

  _NotePainter(
      {required this.frame,
      required this.marks,
      required this.clip,
      required this.scale});

  @override
  void paint(Canvas canvas, Size size) {
    canvas.save();
    canvas.scale(scale);
    canvas.drawImage(frame, Offset.zero, Paint());
    final region = clip;
    if (region != null) {
      final full = Rect.fromLTWH(
          0, 0, frame.width.toDouble(), frame.height.toDouble());
      final shade = Path()
        ..fillType = PathFillType.evenOdd
        ..addRect(full)
        ..addRect(region);
      canvas.drawPath(shade, Paint()..color = Colors.black45);
      canvas.drawRect(
          region,
          Paint()
            ..color = Colors.white
            ..style = PaintingStyle.stroke
            ..strokeWidth = 2 / scale);
    }
    paintMarks(canvas, marks, strokeScale: 1 / scale);
    canvas.restore();
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) => true;
}
