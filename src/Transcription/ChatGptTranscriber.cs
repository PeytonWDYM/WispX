using System;
using System.IO;
using System.Net;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;
using System.Threading.Tasks;

namespace WispX
{
    internal sealed class TranscribeException : Exception
    {
        public TranscribeException(string msg) : base(msg) { }
    }

    /// <summary>
    /// ChatGPT dictation, the same POST https://chatgpt.com/backend-api/transcribe that the
    /// ChatGPT / Codex "Dictate" mic button uses. It signs in with the login Codex already
    /// saved in %USERPROFILE%\.codex\auth.json, so there's nothing extra to sign in to.
    /// </summary>
    internal sealed class ChatGptTranscriber
    {
        private const string Endpoint = "https://chatgpt.com/backend-api/transcribe";
        private const string TokenUrl = "https://auth.openai.com/oauth/token";
        private static readonly string AuthPath =
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".codex", "auth.json");

        private static readonly HttpClient Http = new HttpClient(new SocketsHttpHandler
        {
            AutomaticDecompression = DecompressionMethods.All,
            PooledConnectionLifetime = TimeSpan.FromMinutes(10),
        })
        { Timeout = TimeSpan.FromSeconds(60) };

        private static readonly SemaphoreSlim RefreshLock = new SemaphoreSlim(1, 1);

        static ChatGptTranscriber()
        {
            Http.DefaultRequestHeaders.UserAgent.ParseAdd("WispX/1.0 (Windows)");
        }

        public static bool HasLogin() => TryReadAuth(out _, out _, out _);

        /// <summary>Reads the email in Codex's ID token without refreshing it or contacting a server.</summary>
        public static string LoginEmail()
        {
            try
            {
                if (!HasLogin()) return null;
                var root = JsonNode.Parse(ReadShared(AuthPath));
                return Claim(root?["tokens"]?["id_token"]?.GetValue<string>(), "email");
            }
            catch { return null; }
        }

        /// <summary>Opens a TCP+TLS connection to chatgpt.com ahead of time so the first transcription feels instant.</summary>
        public static void Warmup()
        {
            _ = Task.Run(async () =>
            {
                try { using var r = await Http.SendAsync(new HttpRequestMessage(HttpMethod.Head, "https://chatgpt.com/")); } catch { }
            });
        }

        public async Task<string> TranscribeAsync(byte[] wav, int durationMs, string language)
        {
            if (!TryReadAuth(out var access, out var account, out _))
                throw new TranscribeException("Sign in to the Codex app first");

            if (IsExpired(access)) access = await RefreshAsync(access) ?? access;

            var (status, body) = await PostAsync(wav, durationMs, language, access, account);
            if (status == 401)
            {
                var fresh = await RefreshAsync(access);
                if (fresh != null) (status, body) = await PostAsync(wav, durationMs, language, fresh, account);
            }

            if (status == 200)
            {
                try
                {
                    using var doc = JsonDocument.Parse(body);
                    if (doc.RootElement.TryGetProperty("text", out var t)) return t.GetString() ?? "";
                }
                catch { }
                throw new TranscribeException("Unexpected reply from ChatGPT");
            }
            if (status == 401) throw new TranscribeException("Codex login expired. Open Codex once");
            if (status == 400 && body.Contains("audio_too_short")) throw new TranscribeException("Too short. Hold a bit longer");
            if (status == 403) throw new TranscribeException("ChatGPT refused the request (403)");
            if (status == 413) throw new TranscribeException("Recording too long");
            if (status == 429) throw new TranscribeException("ChatGPT rate limit. Try again soon");
            if (status == -1) throw new TranscribeException("No connection to ChatGPT");
            throw new TranscribeException($"ChatGPT error {status}");
        }

        private static async Task<(int status, string body)> PostAsync(byte[] wav, int durationMs, string language, string access, string account)
        {
            using var form = new MultipartFormDataContent("----wispx-" + Guid.NewGuid().ToString("N"));
            var file = new ByteArrayContent(wav);
            file.Headers.ContentType = new MediaTypeHeaderValue("audio/wav");
            form.Add(file, "file", "wispx.wav");
            form.Add(new StringContent(durationMs.ToString()), "duration_ms");
            if (!string.IsNullOrWhiteSpace(language)) form.Add(new StringContent(language.Trim()), "language");

            using var req = new HttpRequestMessage(HttpMethod.Post, Endpoint) { Content = form };
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", access);
            if (!string.IsNullOrEmpty(account)) req.Headers.TryAddWithoutValidation("chatgpt-account-id", account);
            req.Headers.TryAddWithoutValidation("originator", "codex_desktop");
            try
            {
                using var res = await Http.SendAsync(req);
                return ((int)res.StatusCode, await res.Content.ReadAsStringAsync());
            }
            catch (TaskCanceledException) { throw new TranscribeException("ChatGPT took too long"); }
            catch (HttpRequestException) { return (-1, ""); }
        }

        // ---------- Codex login (read-only unless the token has actually expired) ----------

        private static bool TryReadAuth(out string access, out string account, out string refresh)
        {
            access = account = refresh = null;
            try
            {
                if (!File.Exists(AuthPath)) return false;
                var root = JsonNode.Parse(ReadShared(AuthPath));
                var tokens = root?["tokens"];
                access = tokens?["access_token"]?.GetValue<string>();
                refresh = tokens?["refresh_token"]?.GetValue<string>();
                account = tokens?["account_id"]?.GetValue<string>() ?? AccountFromJwt(access);
                return !string.IsNullOrEmpty(access);
            }
            catch { return false; }
        }

        /// <summary>
        /// Refreshes the Codex login the same way Codex does, then writes the new tokens back
        /// into auth.json (keeping every other field) so Codex keeps working too.
        /// Refresh tokens rotate, so writing them back is what keeps Codex from being signed out.
        /// </summary>
        private static async Task<string> RefreshAsync(string staleAccess)
        {
            await RefreshLock.WaitAsync();
            try
            {
                // Codex may have refreshed it already while we were waiting.
                if (TryReadAuth(out var cur, out _, out var refresh) && cur != staleAccess && !IsExpired(cur)) return cur;
                if (string.IsNullOrEmpty(refresh)) return null;

                string clientId = Claim(cur, "client_id") ?? "app_EMoamEEZ73f0CkXaXp7hrann";
                var payload = JsonSerializer.Serialize(new { client_id = clientId, grant_type = "refresh_token", refresh_token = refresh, scope = "openid profile email" });
                using var res = await Http.PostAsync(TokenUrl, new StringContent(payload, Encoding.UTF8, "application/json"));
                if (!res.IsSuccessStatusCode) return null;
                var json = JsonNode.Parse(await res.Content.ReadAsStringAsync());
                string newAccess = json?["access_token"]?.GetValue<string>();
                if (string.IsNullOrEmpty(newAccess)) return null;

                // Re-read right before writing so nothing Codex changed gets lost.
                var root = JsonNode.Parse(ReadShared(AuthPath)) as JsonObject;
                var tokens = root?["tokens"] as JsonObject;
                if (root == null || tokens == null) return newAccess;
                tokens["access_token"] = newAccess;
                if (json["id_token"] is JsonNode id) tokens["id_token"] = id.GetValue<string>();
                if (json["refresh_token"] is JsonNode rt) tokens["refresh_token"] = rt.GetValue<string>();
                root["last_refresh"] = DateTime.UtcNow.ToString("o");

                var tmp = AuthPath + ".wispx.tmp";
                File.WriteAllText(tmp, root.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));
                File.Move(tmp, AuthPath, overwrite: true);
                return newAccess;
            }
            catch { return null; }
            finally { RefreshLock.Release(); }
        }

        private static string ReadShared(string path)
        {
            using var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            using var sr = new StreamReader(fs);
            return sr.ReadToEnd();
        }

        private static bool IsExpired(string jwt)
        {
            var exp = Claim(jwt, "exp");
            return exp != null && long.TryParse(exp, out var e) && DateTimeOffset.FromUnixTimeSeconds(e) < DateTimeOffset.UtcNow.AddSeconds(60);
        }

        private static string AccountFromJwt(string jwt)
        {
            try
            {
                using var d = JsonDocument.Parse(JwtPayload(jwt));
                if (d.RootElement.TryGetProperty("https://api.openai.com/auth", out var a) &&
                    a.TryGetProperty("chatgpt_account_id", out var id)) return id.GetString();
            }
            catch { }
            return null;
        }

        private static string Claim(string jwt, string name)
        {
            try
            {
                using var d = JsonDocument.Parse(JwtPayload(jwt));
                return d.RootElement.TryGetProperty(name, out var v) ? (v.ValueKind == JsonValueKind.String ? v.GetString() : v.GetRawText()) : null;
            }
            catch { return null; }
        }

        private static string JwtPayload(string jwt)
        {
            var p = jwt.Split('.')[1].Replace('-', '+').Replace('_', '/');
            p = p.PadRight(p.Length + (4 - p.Length % 4) % 4, '=');
            return Encoding.UTF8.GetString(Convert.FromBase64String(p));
        }
    }
}
