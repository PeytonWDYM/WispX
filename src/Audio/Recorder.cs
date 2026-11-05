using System;
using System.IO;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using NAudio.Wave;

namespace WispX
{
    /// <summary>Records a selected microphone to 16 kHz mono PCM and exposes a live level for the waveform.</summary>
    internal sealed class Recorder
    {
        private const int Rate = 16000;
        private WaveInEvent _wave;
        private MemoryStream _pcm;
        private readonly object _gate = new object();
        private ManualResetEventSlim _stopped;
        private volatile float _level;
        private float _peak;

        public float Level => _level;
        public bool IsRecording => _wave != null;

        /// <summary>Names reported by Windows; an empty selection follows its default device.</summary>
        public static IReadOnlyList<string> InputDevices() => Enumerable.Range(0, WaveInEvent.DeviceCount)
            .Select(i => WaveInEvent.GetCapabilities(i).ProductName).ToArray();

        public void Start(string deviceName)
        {
            Cancel();
            _pcm = new MemoryStream();
            _peak = 0; _level = 0;
            _stopped = new ManualResetEventSlim(false);
            int device = -1;
            if (deviceName.Length > 0)
                for (int i = 0; i < WaveInEvent.DeviceCount; i++)
                    if (WaveInEvent.GetCapabilities(i).ProductName == deviceName) { device = i; break; }
            var w = new WaveInEvent
            {
                DeviceNumber = device, // WAVE_MAPPER (-1) follows the Windows default mic
                WaveFormat = new WaveFormat(Rate, 16, 1),
                BufferMilliseconds = 30,
                NumberOfBuffers = 4,
            };
            w.DataAvailable += OnData;
            var stoppedEvt = _stopped;
            w.RecordingStopped += (_, __) => stoppedEvt.Set();
            _wave = w;
            try { w.StartRecording(); }
            catch { Cancel(); throw; }
        }

        private void OnData(object sender, WaveInEventArgs e)
        {
            double sum = 0;
            int n = e.BytesRecorded / 2;
            for (int i = 0; i < e.BytesRecorded - 1; i += 2)
            {
                double s = BitConverter.ToInt16(e.Buffer, i) / 32768.0;
                sum += s * s;
            }
            double rms = n > 0 ? Math.Sqrt(sum / n) : 0;
            double db = 20 * Math.Log10(rms + 1e-9);
            float norm = (float)Math.Clamp((db + 52) / 40, 0, 1);
            _level = norm;
            if (norm > _peak) _peak = norm;
            lock (_gate) _pcm?.Write(e.Buffer, 0, e.BytesRecorded);
        }

        /// <summary>Loudest level heard so far in this recording (0..1).</summary>
        public float Peak => _peak;

        /// <summary>
        /// Stops and returns a WAV file plus the loudest level seen (0..1).
        /// Never blocks the UI: the mic's "stopped" signal is delivered on the UI thread,
        /// so waiting for it there would freeze the overlay until a timeout.
        /// </summary>
        public async System.Threading.Tasks.Task<(byte[] wav, float peak, int ms)> StopAsync()
        {
            var w = _wave;
            if (w == null) return (null, 0, 0);
            _wave = null;
            var stopped = _stopped;
            try { w.StopRecording(); } catch { }
            await System.Threading.Tasks.Task.Run(() => stopped.Wait(400)); // flushes the last buffer
            try { w.Dispose(); } catch { }
            byte[] pcm;
            lock (_gate) { pcm = _pcm.ToArray(); _pcm = null; }
            _level = 0;
            return (ToWav(pcm), _peak, pcm.Length * 1000 / (Rate * 2));
        }

        public void Cancel()
        {
            var w = _wave;
            _wave = null;
            if (w != null) { try { w.StopRecording(); } catch { } w.Dispose(); }
            lock (_gate) _pcm = null;
            _level = 0;
        }

        private static byte[] ToWav(byte[] pcm)
        {
            using var ms = new MemoryStream(44 + pcm.Length);
            using var bw = new BinaryWriter(ms);
            bw.Write("RIFF"u8.ToArray()); bw.Write(36 + pcm.Length);
            bw.Write("WAVE"u8.ToArray());
            bw.Write("fmt "u8.ToArray()); bw.Write(16); bw.Write((short)1); bw.Write((short)1);
            bw.Write(Rate); bw.Write(Rate * 2); bw.Write((short)2); bw.Write((short)16);
            bw.Write("data"u8.ToArray()); bw.Write(pcm.Length); bw.Write(pcm);
            bw.Flush();
            return ms.ToArray();
        }
    }
}
