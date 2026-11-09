import 'dart:async';
import 'dart:math' as math;
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/desktop/widgets/agent_overlay.dart';
import 'package:flutter_hbb/models/agent_bridge.dart';
import 'package:flutter_hbb/models/agent_model.dart';
import 'package:flutter_hbb/models/agent_note.dart';
import 'package:flutter_hbb/models/model.dart';
import 'package:flutter_hbb/models/platform_model.dart';

const _agentGreen = Color(0xFF22C55E);
const _stopRed = Color(0xFFDC2626);

/// The remote page keeps the soft keyboard off for its own input; a text field of the panel turns it back on.
void _allowSoftKeyboard() {
  if (isAndroid) gFFI.invokeMethod("enable_soft_keyboard", true);
}

/// The agent panel on the phone's remote page: the desktop overlay's controls (AgentOverlay), laid out for touch.
/// The badge opens a sheet with Share, Hand to agent / Take back, the message box, Block, End share and the bridge
/// settings; Stop stays on screen while an agent holds the share. The bridge already takes input back on a GUI's
/// touch; the panel's own take-back on touch is a fallback.
class MobileAgentPanel extends StatefulWidget {
  final FFI ffi;

  const MobileAgentPanel({Key? key, required this.ffi}) : super(key: key);

  @override
  State<MobileAgentPanel> createState() => _MobileAgentPanelState();
}

class _MobileAgentPanelState extends State<MobileAgentPanel> {
  FFI get ffi => widget.ffi;

  /// The same per-PC setting the desktop panel keeps.
  String get _hostKey => 'rigdesk-agent-host-${ffi.id}';

  AgentBridge? _bridge;

  @override
  void initState() {
    super.initState();
    final saved = AgentHost.fromJson(bind.mainGetLocalOption(key: _hostKey));
    if (saved != null) _connect(saved);
  }

  /// The page can rebuild this panel in a new place before the old one is disposed, so each one drops only its own
  /// bridge from the shared map.
  void _drop() {
    final key = '${ffi.sessionId}';
    if (identical(agentBridges[key], _bridge)) agentBridges.remove(key);
    _bridge?.close();
    _bridge = null;
  }

  @override
  void dispose() {
    _drop();
    super.dispose();
  }

  void _connect(AgentHost host) {
    _drop();
    final bridge = AgentBridge(host,
        onState: (state) => onAgentStateEvent(ffi.sessionId, state),
        onTrouble: showToast);
    agentBridges['${ffi.sessionId}'] = bridge;
    _bridge = bridge;
    bridge.start();
    if (mounted) setState(() {});
  }

  void _ask(String verb) => _bridge?.ask(verb);

  Future<void> _setUp() async {
    final saved = AgentHost.fromJson(bind.mainGetLocalOption(key: _hostKey));
    _allowSoftKeyboard();
    final host = await showDialog<AgentHost>(
        context: context,
        builder: (_) => Listener(
            onPointerDown: (_) => _allowSoftKeyboard(),
            child: AgentHostDialog(initial: saved, peer: ffi.id)));
    if (host == null) return;
    await bind.mainSetLocalOption(key: _hostKey, value: host.toJson());
    _connect(host);
  }

  void _message() {
    final bridge = _bridge;
    if (bridge == null) return;
    Navigator.of(context).push(MaterialPageRoute(
        fullscreenDialog: true,
        builder: (_) => MobileAgentNotePage(ffi: ffi, bridge: bridge)));
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
                if (agent.agentHolds) _ask('take_back');
              },
            ),
          ),
        ];
        final cursor = _cursor(agent);
        if (cursor != null) children.add(cursor);
        children.add(Positioned(
          left: 8,
          top: 8,
          right: 8,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Wrap(
                spacing: 8,
                runSpacing: 8,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  _badge(agent),
                  if (agent.isShared) _stopButton(),
                ],
              ),
              if (agent.isShared && agent.actions.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: IgnorePointer(child: _strip(agent)),
                ),
            ],
          ),
        ));
        return Stack(children: children);
      },
    );
  }

  Widget _badge(AgentModel agent) {
    final String label;
    if (_bridge == null) {
      label = 'Set up bridge';
    } else if (agent.isShared) {
      label = '${agent.ownerTitle} · ${_holds(agent)}';
    } else if (agent.linkOpen) {
      label = 'Waiting for the agent';
    } else {
      label = 'Agent';
    }
    return Material(
      elevation: 4,
      color: agent.isBlocked ? Colors.orange.shade100 : null,
      borderRadius: BorderRadius.circular(20),
      child: InkWell(
        borderRadius: BorderRadius.circular(20),
        onTap: _bridge == null ? _setUp : _openSheet,
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
          child: Row(mainAxisSize: MainAxisSize.min, children: [
            Icon(Icons.smart_toy,
                size: 18, color: agent.isShared ? _agentGreen : null),
            const SizedBox(width: 6),
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 200),
              child: Text(label, overflow: TextOverflow.ellipsis),
            ),
          ]),
        ),
      ),
    );
  }

  static String _holds(AgentModel agent) => agent.isBlocked
      ? 'blocked'
      : agent.agentHolds
          ? 'has input'
          : 'waiting';

  Widget _stopButton({bool wide = false}) {
    final blocked = AgentModel.of(ffi.sessionId).isBlocked;
    return ElevatedButton.icon(
      style: ElevatedButton.styleFrom(
        backgroundColor: blocked ? _agentGreen : _stopRed,
        foregroundColor: Colors.white,
        minimumSize: wide ? const Size.fromHeight(56) : null,
        padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 14),
        textStyle: const TextStyle(fontSize: 18, fontWeight: FontWeight.bold),
      ),
      onPressed: () => _ask(blocked ? 'unblock' : 'stop'),
      icon: Icon(blocked ? Icons.play_circle : Icons.stop_circle),
      label: Text(blocked ? 'Resume' : 'Stop'),
    );
  }

  void _openSheet() {
    showModalBottomSheet(
      context: context,
      isScrollControlled: true,
      builder: (sheet) {
        final agent = AgentModel.of(ffi.sessionId);
        void run(VoidCallback act) {
          Navigator.of(sheet).pop();
          act();
        }

        return SafeArea(
          child: AnimatedBuilder(
            animation: agent,
            builder: (_, __) => SingleChildScrollView(
              padding: const EdgeInsets.symmetric(vertical: 8),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  if (agent.isShared)
                    ListTile(
                      leading:
                          const Icon(Icons.smart_toy, color: _agentGreen),
                      title: Text(agent.ownerTitle),
                      subtitle: Text(
                          'Rig agent in ${agent.ownerGraph} · ${_holds(agent)}'),
                    ),
                  ListTile(
                    leading: const Icon(Icons.share),
                    title: Text(agent.linkOpen && !agent.isShared
                        ? 'Link copied, waiting for the agent'
                        : 'Share with agent'),
                    subtitle: const Text(
                        'Copy a one-time link that hands this session to a Rig agent'),
                    onTap: () => run(() => _ask('share')),
                  ),
                  if (agent.isShared && agent.holder == 'human')
                    ListTile(
                      leading: const Icon(Icons.pan_tool_alt),
                      title: const Text('Hand to agent'),
                      onTap: () => run(() => _ask('hand')),
                    ),
                  if (agent.agentHolds)
                    ListTile(
                      leading: const Icon(Icons.back_hand),
                      title: const Text('Take back'),
                      onTap: () => run(() => _ask('take_back')),
                    ),
                  if (agent.isShared)
                    ListTile(
                      leading: const Icon(Icons.chat_bubble_outline),
                      title: const Text('Message the agent'),
                      subtitle: const Text('With a screenshot to clip and mark'),
                      onTap: () => run(_message),
                    ),
                  if (agent.isShared)
                    ListTile(
                      leading: Icon(agent.isBlocked
                          ? Icons.play_circle_outline
                          : Icons.block),
                      title: Text(agent.isBlocked ? 'Unblock' : 'Block'),
                      onTap: () => run(
                          () => _ask(agent.isBlocked ? 'unblock' : 'block')),
                    ),
                  if (agent.isShared)
                    ListTile(
                      leading: const Icon(Icons.link_off),
                      title: const Text('End share'),
                      onTap: () => run(() => _ask('revoke')),
                    ),
                  ListTile(
                    leading: const Icon(Icons.settings),
                    title: const Text('Bridge settings'),
                    onTap: () => run(_setUp),
                  ),
                  if (agent.isShared)
                    Padding(
                      padding: const EdgeInsets.fromLTRB(16, 8, 16, 8),
                      child: _stopButton(wide: true),
                    ),
                ],
              ),
            ),
          ),
        );
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
              constraints: const BoxConstraints(maxWidth: 220),
              padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
              decoration: BoxDecoration(
                color: _agentGreen,
                borderRadius: BorderRadius.circular(4),
              ),
              child: Text(
                '${point.agentTitle}: ${point.summary}',
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(color: Colors.white, fontSize: 12),
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _strip(AgentModel agent) {
    final recent = agent.actions.length > 3
        ? agent.actions.sublist(agent.actions.length - 3)
        : agent.actions;
    return Container(
      constraints: const BoxConstraints(maxWidth: 320),
      padding: const EdgeInsets.all(6),
      decoration: BoxDecoration(
        color: Colors.black.withOpacity(0.6),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: recent
            .map((a) => Text(
                  '${a.ok ? '✓' : '✗'} ${a.agentTitle}: ${a.summary}',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                      color: a.ok ? Colors.white : Colors.orangeAccent,
                      fontSize: 11),
                ))
            .toList(),
      ),
    );
  }
}

/// The message box on the phone: a full-screen page with a still of the PC's screen that fits the screen, the clip
/// and drawing tools of the desktop box (agent_note.dart), and the words for the agent.
class MobileAgentNotePage extends StatefulWidget {
  final FFI ffi;
  final AgentBridge bridge;

  const MobileAgentNotePage({Key? key, required this.ffi, required this.bridge})
      : super(key: key);

  @override
  State<MobileAgentNotePage> createState() => _MobileAgentNotePageState();
}

class _MobileAgentNotePageState extends State<MobileAgentNotePage> {
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
  double _viewScale = 1;

  @override
  void initState() {
    super.initState();
    _allowSoftKeyboard();
    _fetch();
  }

  @override
  void dispose() {
    _frame?.dispose();
    _text.dispose();
    super.dispose();
  }

  Future<void> _fetch() async {
    final agent = AgentModel.of(widget.ffi.sessionId);
    final waiter = Completer<Uint8List?>();
    agent.frameWaiter = waiter;
    final display = widget.ffi.ffiModel.pi.currentDisplay;
    await widget.bridge
        .ask('frame', display: display == kAllDisplayValue ? 0 : display);
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
      } catch (_) {
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
    _allowSoftKeyboard();
    final text = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Label'),
        content: TextField(
          controller: controller,
          autofocus: true,
          onTap: _allowSoftKeyboard,
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
    setState(
        () => _marks.add(NoteMark(NoteTool.text, [at], text: text.trim())));
  }

  Future<void> _send() async {
    final words = _text.text.trim();
    if (words.isEmpty || _sending) return;
    setState(() => _sending = true);
    final frame = _frame;
    final png = frame == null
        ? Uint8List(0)
        : await bakeNote(frame, _clip, _marks, strokeScale: 1 / _viewScale);
    await widget.bridge.ask('note', text: words, png: png);
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
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      padding: const EdgeInsets.symmetric(horizontal: 8),
      child: Row(
        children: [
          for (final entry in labels.entries)
            Padding(
              padding: const EdgeInsets.only(right: 6),
              child: ChoiceChip(
                label: Text(entry.value),
                selected: _tool == entry.key,
                onSelected: (_) => setState(() => _tool = entry.key),
              ),
            ),
          TextButton(
            onPressed: _marks.isEmpty
                ? null
                : () => setState(() => _marks.removeLast()),
            child: const Text('Undo'),
          ),
          TextButton(
            onPressed:
                _clip == null ? null : () => setState(() => _clip = null),
            child: const Text('Whole screen'),
          ),
        ],
      ),
    );
  }

  Widget _canvas() {
    final frame = _frame;
    if (_loading) return const Center(child: CircularProgressIndicator());
    if (frame == null) {
      return Center(
          child: Padding(
              padding: const EdgeInsets.all(16), child: Text(_error)));
    }
    return LayoutBuilder(builder: (context, area) {
      _viewScale = math.min(area.maxWidth / frame.width,
          area.maxHeight / frame.height);
      final s = _viewScale;
      final drawing = _drawing;
      return Center(
        child: SizedBox(
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
              painter: _MobileNotePainter(
                frame: frame,
                marks: [..._marks, if (drawing != null) drawing],
                clip: _clip,
                scale: s,
              ),
            ),
          ),
        ),
      );
    });
  }

  @override
  Widget build(BuildContext context) {
    return Listener(
      onPointerDown: (_) => _allowSoftKeyboard(),
      child: Scaffold(
        appBar: AppBar(
          title: Text(
              'Message ${AgentModel.of(widget.ffi.sessionId).ownerTitle}'),
        ),
        body: SafeArea(
          child: Column(
            children: [
              if (_frame != null) _tools(),
              Expanded(child: _canvas()),
              Padding(
                padding: const EdgeInsets.fromLTRB(8, 4, 8, 8),
                child: Row(
                  children: [
                    Expanded(
                      child: TextField(
                        controller: _text,
                        minLines: 1,
                        maxLines: 4,
                        decoration: const InputDecoration(
                            hintText: 'What should the agent know or do?'),
                      ),
                    ),
                    IconButton(
                      tooltip: _clip == null
                          ? 'Send with screenshot'
                          : 'Send with clip',
                      onPressed: _sending ? null : _send,
                      icon: const Icon(Icons.send),
                    ),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _MobileNotePainter extends CustomPainter {
  final ui.Image frame;
  final List<NoteMark> marks;
  final Rect? clip;
  final double scale;

  _MobileNotePainter(
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
