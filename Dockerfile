FROM eclipse-temurin:21 AS mojangapi-builder

# Get mojangapi data
COPY --chown=root:root . /mojangapi

# Build mojangapi
WORKDIR /mojangapi
RUN --mount=type=cache,target=/root/.gradle,sharing=locked --mount=type=cache,target=/mojangapi/.gradle,sharing=locked --mount=type=cache,target=/mojangapi/work,sharing=locked \
    ./gradlew build --stacktrace

FROM eclipse-temurin:21.0.5_11-jdk-alpine AS jre-no-javac-builder

# Install necessery dependencies
RUN apk add --no-progress --no-cache binutils tzdata

ARG JAVA_MODULES="java.base,java.compiler,java.instrument,java.logging,java.management,java.net.http,java.sql,java.desktop,java.security.sasl,java.naming,java.transaction.xa,java.xml,jdk.crypto.ec,jdk.incubator.vector,jdk.jfr,jdk.zipfs,jdk.security.auth,jdk.unsupported,jdk.management"

# Create a custom Java runtime for mojangapi Server
RUN jlink \
        --add-modules $JAVA_MODULES \
        --strip-debug \
        --no-man-pages \
        --no-header-files \
        --compress=2 \
        --output /mojangapi/java

FROM alpine:latest AS mojangapi-runner

# Setup groups and install dumb init
RUN addgroup --gid 1001 mojangapi && \
    adduser --home /mojangapi --uid 1001 -S -G mojangapi mojangapi && \
    apk add --update --no-progress --no-cache dumb-init libstdc++

# Setting up Java
ENV JAVA_HOME=/opt/java/openjdk \
    PATH="/opt/java/openjdk/bin:$PATH"

# Copy over JRE
COPY --from=jre-no-javac-builder --chown=mojangapi:mojangapi /mojangapi/java $JAVA_HOME
COPY --from=mojangapi-builder --chown=mojangapi:mojangapi /mojangapi/build/distributions/*.zip /mojangapi/mojangapi.zip

# Unzip the mojangapi
RUN unzip /mojangapi/mojangapi.zip -d /mojangapi && \
    rm /mojangapi/mojangapi.zip

# Use the mojangapi's home directory as our work directory
WORKDIR /mojangapi

# Switch from root to mojangapi
USER mojangapi

EXPOSE 8080/tcp

# Start the process using dumb-init
ENTRYPOINT ["/usr/bin/dumb-init", "--"]
CMD ["/mojangapi/mojangapi/bin/mojangapi"]
