using System;
using System.IO;
using System.Media;

namespace WispX
{
    /// <summary>
    /// Soft, deep "felt tap" cues, made in memory so there are no sound files to ship.
    /// Steady pitch (no glide), warm low tone, cushioned attack, quick quiet fade.
    /// </summary>
    internal static class Sounds
    {
        private static readonly byte[] StartWav = WoodBlock(220, 0.09);   // a little higher, "on"
        private static readonly byte[] StopWav = WoodBlock(150, 0.085);   // deeper, "done"
        private static readonly byte[] CancelWav = WoodBlock(120, 0.06);  // deepest and quietest, "never mind"

        /// <summary>
        /// A soft wood block "tok". Real wood rings at a few uneven (inharmonic) overtones that die
        /// out fast, highest first, and the mallet adds a tiny muffled tap. That mix is what makes it
        /// sound hollow and woody instead of like a beep.
        /// </summary>
        private static byte[] WoodBlock(double hz, double vol, double sec = 0.08)
        {
            const int rate = 44100;
            int total = (int)(sec * rate);
            var buf = new double[total];

            // (frequency ratio, loudness, how fast it dies per second)
            var modes = new (double ratio, double amp, double decay)[]
            {
                (1.00, 1.00, 70),   // short and dry: no ringing tail
                (2.76, 0.30, 120),
                (5.40, 0.08, 220),
            };
            var phases = new double[modes.Length];

            var rng = new Random(7);
            double noiseLp = 0, lp1 = 0, lp2 = 0;
            double aNoise = 1 - Math.Exp(-2 * Math.PI * 1800.0 / rate);
            double aOut = 1 - Math.Exp(-2 * Math.PI * 3200.0 / rate); // keeps it soft, not clacky
            const double attack = 0.0015;
            double peak = 1e-9;

            for (int i = 0; i < total; i++)
            {
                double t = (double)i / rate;
                double settle = 1 + 0.05 * Math.Exp(-t / 0.004); // the wood "gives" for a moment when struck
                double s = 0;
                for (int m = 0; m < modes.Length; m++)
                {
                    phases[m] += 2 * Math.PI * hz * modes[m].ratio * settle / rate;
                    s += Math.Sin(phases[m]) * modes[m].amp * Math.Exp(-t * modes[m].decay);
                }

                noiseLp += ((rng.NextDouble() * 2 - 1) - noiseLp) * aNoise;
                s += noiseLp * 0.9 * Math.Exp(-t / 0.0025); // mallet tap

                double atk = t < attack ? 0.5 - 0.5 * Math.Cos(Math.PI * t / attack) : 1;
                double tail = Math.Clamp((sec - t) / 0.012, 0, 1);
                s *= atk * tail;

                lp1 += (s - lp1) * aOut;
                lp2 += (lp1 - lp2) * aOut;
                buf[i] = lp2;
                peak = Math.Max(peak, Math.Abs(lp2));
            }

            var ms = new MemoryStream();
            var bw = new BinaryWriter(ms);
            bw.Write("RIFF"u8.ToArray()); bw.Write(36 + total * 2); bw.Write("WAVE"u8.ToArray());
            bw.Write("fmt "u8.ToArray()); bw.Write(16); bw.Write((short)1); bw.Write((short)1);
            bw.Write(rate); bw.Write(rate * 2); bw.Write((short)2); bw.Write((short)16);
            bw.Write("data"u8.ToArray()); bw.Write(total * 2);
            foreach (var v in buf) bw.Write((short)(Math.Clamp(v / peak * vol, -1, 1) * 32767)); // same loudness as before
            bw.Flush();
            return ms.ToArray();
        }

        private static readonly SoundPlayer StartP = Player(StartWav);
        private static readonly SoundPlayer StopP = Player(StopWav);
        private static readonly SoundPlayer CancelP = Player(CancelWav);

        public static bool Enabled { get; set; } = true;
        public static void Start() => Play(StartP);
        public static void Stop() => Play(StopP);
        public static void Cancel() => Play(CancelP);

        private static void Play(SoundPlayer p) { if (Enabled) try { p.Play(); } catch { } }

        /// <summary>Writes the cues as .wav files so they can be previewed.</summary>
        public static void Export(string dir)
        {
            Directory.CreateDirectory(dir);
            File.WriteAllBytes(Path.Combine(dir, "wispx-start.wav"), StartWav);
            File.WriteAllBytes(Path.Combine(dir, "wispx-stop.wav"), StopWav);
            File.WriteAllBytes(Path.Combine(dir, "wispx-cancel.wav"), CancelWav);
        }

        private static SoundPlayer Player(byte[] wav)
        {
            var p = new SoundPlayer(new MemoryStream(wav));
            p.Load();
            return p;
        }

        /// <summary>
        /// A deep, soft "thock": a low body that thuds into place (quick pitch settle), a tiny
        /// muffled felt transient on top, everything low-passed, and gone in ~120 ms.
        /// </summary>
        private static byte[] Thock(double hz, double vol, double sec = 0.14)
        {
            const int rate = 44100;
            int total = (int)(sec * rate);
            var ms = new MemoryStream();
            var bw = new BinaryWriter(ms);
            bw.Write("RIFF"u8.ToArray()); bw.Write(36 + total * 2); bw.Write("WAVE"u8.ToArray());
            bw.Write("fmt "u8.ToArray()); bw.Write(16); bw.Write((short)1); bw.Write((short)1);
            bw.Write(rate); bw.Write(rate * 2); bw.Write((short)2); bw.Write((short)16);
            bw.Write("data"u8.ToArray()); bw.Write(total * 2);

            var rng = new Random(42);
            double phase = 0, noiseLp = 0, outLp1 = 0, outLp2 = 0;
            double aNoise = 1 - Math.Exp(-2 * Math.PI * 700.0 / rate);  // muffled, felt-like click
            double aOut = 1 - Math.Exp(-2 * Math.PI * 1400.0 / rate);   // two poles: rounds off every edge
            const double attack = 0.0025;

            for (int i = 0; i < total; i++)
            {
                double t = (double)i / rate;

                // Body: starts a little higher and drops onto the note within ~12 ms. That's the "th" in thock.
                double f = hz * (1 + 0.7 * Math.Exp(-t / 0.012));
                phase += 2 * Math.PI * f / rate;
                double body = Math.Sin(phase) + 0.18 * Math.Sin(2 * phase) * Math.Exp(-t * 60);
                double bodyEnv = Math.Exp(-t * 30);

                // Felt transient: a few ms of soft, filtered noise.
                noiseLp += ((rng.NextDouble() * 2 - 1) - noiseLp) * aNoise;
                double click = noiseLp * 2.2 * Math.Exp(-t / 0.004);

                double atk = t < attack ? 0.5 - 0.5 * Math.Cos(Math.PI * t / attack) : 1;
                double tail = Math.Clamp((sec - t) / 0.02, 0, 1);
                double s = (body * bodyEnv + click) * atk * tail;

                outLp1 += (s - outLp1) * aOut;
                outLp2 += (outLp1 - outLp2) * aOut;
                bw.Write((short)(Math.Clamp(outLp2 * vol, -1, 1) * 32767));
            }
            bw.Flush();
            return ms.ToArray();
        }

        private static byte[] Tap(double hz, double vol, double sec)
        {
            const int rate = 44100;
            int total = (int)(sec * rate);
            var ms = new MemoryStream();
            var bw = new BinaryWriter(ms);
            bw.Write("RIFF"u8.ToArray()); bw.Write(36 + total * 2); bw.Write("WAVE"u8.ToArray());
            bw.Write("fmt "u8.ToArray()); bw.Write(16); bw.Write((short)1); bw.Write((short)1);
            bw.Write(rate); bw.Write(rate * 2); bw.Write((short)2); bw.Write((short)16);
            bw.Write("data"u8.ToArray()); bw.Write(total * 2);

            const double attack = 0.007;           // cushioned, so it's soft rather than a sharp click
            double lp = 0;                          // one-pole low-pass to shave off any brightness
            double a = 1 - Math.Exp(-2 * Math.PI * 900.0 / rate);
            for (int i = 0; i < total; i++)
            {
                double t = (double)i / rate;
                double env = t < attack ? 0.5 - 0.5 * Math.Cos(Math.PI * t / attack) : Math.Exp(-(t - attack) * 16);
                double tail = Math.Min(1, (sec - t) / 0.03); // fade the last 30 ms to true silence
                env *= Math.Max(0, tail);

                // Warm body: fundamental, a gentle octave that fades fast (gives it "felt"), and a sub for depth.
                double s = Math.Sin(2 * Math.PI * hz * t)
                         + 0.22 * Math.Sin(2 * Math.PI * hz * 2 * t) * Math.Exp(-t * 45)
                         + 0.35 * Math.Sin(2 * Math.PI * hz * 0.5 * t);
                lp += (s - lp) * a;
                bw.Write((short)(Math.Clamp(lp * env * vol, -1, 1) * 32767));
            }
            bw.Flush();
            return ms.ToArray();
        }
    }
}
