import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

void main() {
  runApp(const OsuPreviewApp());
}

class OsuPreviewApp extends StatelessWidget {
  const OsuPreviewApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'osu! preview',
      theme: ThemeData.dark(useMaterial3: true),
      home: const PreviewPage(),
    );
  }
}

class PreviewPage extends StatefulWidget {
  const PreviewPage({super.key});

  @override
  State<PreviewPage> createState() => _PreviewPageState();
}

class _PreviewPageState extends State<PreviewPage> {
  static const _channel = MethodChannel('io.github.astral.osu/render');

  final _bidController = TextEditingController();
  int? _textureId;
  int _mode = 0;
  double _speed = 1.0;
  bool _playing = false;
  String _status = '输入谱面 bid（比如 450410）然后点加载';
  bool _loading = false;
  Timer? _clockTimer;
  final List<String> _logs = [];

  static const _modeNames = ['std', 'taiko', 'catch', 'mania'];

  @override
  void initState() {
    super.initState();
    // 接收 Kotlin 侧反向推送（下载进度/渲染诊断/错误详情）
    _channel.setMethodCallHandler((call) async {
      if (call.method == 'status') {
        final raw = (call.arguments as Map?)?['text'];
        if (raw is String && mounted) {
          setState(() {
            for (final rawLine in raw.split('\n')) {
              if (rawLine.trim().isEmpty) continue;
              final line =
                  rawLine.length > 80 ? rawLine.substring(0, 80) : rawLine;
              _logs.add(line);
            }
            while (_logs.length > 14) {
              _logs.removeAt(0);
            }
          });
        }
      }
      return null;
    });
  }

  @override
  void dispose() {
    _clockTimer?.cancel();
    _bidController.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final bid = int.tryParse(_bidController.text.trim());
    if (bid == null) {
      setState(() => _status = 'bid 必须是数字喵');
      return;
    }
    setState(() {
      _loading = true;
      _status = '下载并解析中…';
      _logs.clear();
    });
    try {
      final result = await _channel.invokeMapMethod<String, dynamic>(
          'loadByBid', {'bid': bid});
      if (result == null) throw Exception('empty result');
      setState(() {
        _textureId = result['textureId'] as int;
        _playing = true;
        _status = '已加载，音频由 ExoPlayer 播，画面由 Rust wgpu 画（bid=$bid）';
      });
      _startClock();
    } on PlatformException catch (e) {
      setState(() => _status = '加载失败: ${e.message}');
    } catch (e) {
      setState(() => _status = '加载失败: $e');
    } finally {
      setState(() => _loading = false);
    }
  }

  void _startClock() {
    _clockTimer?.cancel();
    _clockTimer = Timer.periodic(const Duration(milliseconds: 250), (_) async {
      try {
        final t = await _channel.invokeMethod<int>('positionMs');
        if (mounted && t != null) {
          setState(() => _status =
              '${(t ~/ 60000).toString().padLeft(2, '0')}:${((t ~/ 1000) % 60).toString().padLeft(2, '0')} / $_modeNames[$_mode] / ${_speed.toStringAsFixed(2)}x');
        }
      } catch (_) {
        // channel calls before load are fine to ignore
      }
    });
  }

  Future<void> _invoke(String method, [dynamic args]) async {
    try {
      await _channel.invokeMethod(method, args);
    } catch (_) {}
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('osu! preview (wgpu)')),
      body: Column(
        children: [
          Padding(
            padding: const EdgeInsets.all(12),
            child: Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _bidController,
                    keyboardType: TextInputType.number,
                    decoration: const InputDecoration(
                      labelText: 'beatmap id',
                      border: OutlineInputBorder(),
                    ),
                    onSubmitted: (_) => _load(),
                  ),
                ),
                const SizedBox(width: 8),
                FilledButton(
                  onPressed: _loading ? null : _load,
                  child: Text(_loading ? '加载中…' : '加载'),
                ),
              ],
            ),
          ),
          Expanded(
            child: Center(
              child: _textureId == null
                  ? Text(_status, textAlign: TextAlign.center)
                  : AspectRatio(
                      aspectRatio: 4 / 3,
                      child: Texture(textureId: _textureId!),
                    ),
            ),
          ),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 12),
            child: Column(
              children: [
                if (_logs.isNotEmpty)
                  Container(
                    width: double.infinity,
                    padding: const EdgeInsets.all(6),
                    decoration: BoxDecoration(
                      color: Colors.black.withValues(alpha: 0.4),
                      borderRadius: BorderRadius.circular(6),
                    ),
                    child: Text(
                      _logs.join('\n'),
                      style: const TextStyle(
                          fontSize: 10, fontFamily: 'monospace'),
                      maxLines: 8,
                      overflow: TextOverflow.ellipsis,
                    ),
                  ),
                Text(_status, style: const TextStyle(fontSize: 12)),
                const SizedBox(height: 8),
                Row(
                  children: [
                    IconButton(
                      icon: Icon(_playing ? Icons.pause : Icons.play_arrow),
                      onPressed: () {
                        _playing = !_playing;
                        _invoke(_playing ? 'play' : 'pause');
                        setState(() {});
                      },
                    ),
                    const Spacer(),
                    DropdownButton<int>(
                      value: _mode,
                      items: List.generate(
                          4,
                          (i) => DropdownMenuItem(
                              value: i, child: Text(_modeNames[i]))),
                      onChanged: (v) {
                        if (v == null) return;
                        setState(() => _mode = v);
                        _invoke('setMode', v);
                      },
                    ),
                    const Spacer(),
                    Text('${_speed.toStringAsFixed(2)}x'),
                    SizedBox(
                      width: 140,
                      child: Slider(
                        value: _speed,
                        min: 0.5,
                        max: 1.5,
                        onChanged: (v) {
                          setState(() => _speed = v);
                          _invoke('setSpeed', (v * 100).round());
                        },
                      ),
                    ),
                  ],
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}
