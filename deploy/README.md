# Deploy WorkHub lên server (Hetzner + Cloudflare)

Mô hình hiện tại:

```
trình duyệt ──https──▶ Cloudflare (TLS, proxy) ──http:80──▶ nginx (stack ~/projects)
                                                              │ server_name workhub.buihoangthai.com
                                                              ▼
                                         network projects_app_network, host "workhub"
                                                              │
                                ~/workhub (compose project "workhub")
                                  app: wh serve --auth [--foreman]   :7070 (không publish ra host)
                                  db:  postgres:17-alpine            (chỉ trong network riêng)
```

- Image build ở máy local (bookworm, nên chạy được trên glibc cũ hơn) rồi đẩy qua SSH (`docker save | docker load`), không cần registry. Server 1 vCPU / 2 GB không build Rust nổi.
- Đăng nhập bắt buộc (`--auth`): user lưu trong bảng `users` (argon2id), cookie `wh_session` HttpOnly + Secure + SameSite=Strict, sai 5 lần trong 15 phút thì IP bị khoá. Request ghi (POST) từ origin khác `WH_PUBLIC_URL` bị từ chối.
- Volume: `pgdata` (DB), `data` (worktree, transcript), `repos` (repo cho agent làm), `home` (cấu hình claude/gh/git/ssh của agent).

## Lần đầu

1. **DNS (Cloudflare)**: thêm bản ghi `A workhub → 5.223.75.61`, bật Proxied (đám mây cam). SSL/TLS mode giữ như các site đang chạy (origin chỉ có :80).
2. **`.env` trên server**:
   ```bash
   ssh hetzner_sport_match 'mkdir -p ~/workhub && cd ~/workhub && test -f .env || { cp /dev/null .env; chmod 600 .env; }'
   # nội dung theo deploy/env.example; POSTGRES_PASSWORD=$(openssl rand -hex 24)
   ```
3. **Deploy**: `deploy/deploy.sh` (từ máy local, ở gốc repo).
4. **nginx**: copy `deploy/nginx-workhub.conf` vào `~/projects/nginx/conf.d/production/workhub.conf` (để lần build nginx sau vẫn có), rồi nạp nóng, không làm gián đoạn các site khác:
   ```bash
   docker cp ~/projects/nginx/conf.d/production/workhub.conf projects-nginx-1:/etc/nginx/conf.d/workhub.conf
   docker exec projects-nginx-1 nginx -t && docker exec projects-nginx-1 nginx -s reload
   ```
5. **Tạo user đăng nhập** (nhập password trên terminal, tối thiểu 12 ký tự):
   ```bash
   ssh -t hetzner_sport_match 'cd ~/workhub && docker compose exec app wh user add thai'
   ```

## Cập nhật

`deploy/deploy.sh`: build, đẩy image, `docker compose up -d --wait`, giữ lại image trước đó để rollback.
Rollback: sửa `WH_IMAGE=` trong `~/workhub/.env` về tag cũ (`docker images workhub`) rồi `docker compose up -d`.

## Bật pipeline (agent chạy thật, tốn tiền)

Mặc định `WH_FOREMAN=0`: chỉ có UI/API (xem, tạo task, duyệt). Muốn agent chạy:

1. Auth cho Claude Code: `ANTHROPIC_API_KEY=` hoặc `CLAUDE_CODE_OAUTH_TOKEN=` (lấy bằng `claude setup-token`) trong `.env`.
2. GitHub: `GH_TOKEN=` (fine-grained PAT, quyền contents + pull requests + issues trên repo đích) và để git dùng nó:
   `docker compose exec app gh auth setup-git`.
3. Clone repo đích vào volume và trỏ factory vào đó:
   `docker compose exec app git clone https://github.com/<owner>/<repo> /repos/<repo>`,
   `WH_FACTORY=/repos/<repo>/factory/factory.yaml`.
4. `WH_FOREMAN=1`, `WH_MAX_PARALLEL=1`, rồi `docker compose up -d`.

Agent không bao giờ nhận `DATABASE_URL` của app. Factory có `task_database: { admin_url_env: WH_AGENT_DB_ADMIN_URL }` thì mỗi task có database riêng `wh_task_*` trên Postgres của compose (biến này đã đặt sẵn trong `docker-compose.yml`).

Lưu ý: agent chạy ngay trong container app (sandbox `worktree`); `sandbox: { kind: docker }` không dùng được ở đây. Server chỉ có khoảng 600 MB RAM trống và không có swap, mỗi phiên Claude Code tốn vài trăm MB. Nên giữ `WH_MAX_PARALLEL=1`, thêm swap (cần sudo) hoặc nâng server trước khi chạy nhiều. Factory dogfood (`factory/factory.yaml`) chạy `cargo build/test`, server này không kham nổi.

## Vận hành

```bash
cd ~/workhub
docker compose logs -f app
docker compose exec app wh user list | passwd <u> | remove <u>
docker compose exec app wh task list
docker compose exec db pg_dump -U workhub workhub | gzip > ~/workhub-$(date +%F).sql.gz   # backup
```
