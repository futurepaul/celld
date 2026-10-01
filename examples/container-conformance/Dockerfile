FROM python:3.12-alpine
COPY server.py /srv/server.py
# celld on Docker off Linux reaches only the ports an image exposes.
EXPOSE 8080
CMD ["python3", "-u", "/srv/server.py"]
